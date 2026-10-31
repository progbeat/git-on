//! Wire-level callback tests. A SOCK_SEQPACKET peer substitutes only for the
//! kernel transport; fuser parses actual FUSE 7.31 requests and invokes Overlay.
//! This is not a real mount (see integration.py). It makes kernel-short-circuited
//! callbacks, malformed handles and caller credentials reproducibly testable.
#![cfg(target_os = "linux")]
use fuser::{Session, SessionACL};
use git2::Repository;
use git_on::backend::linux::Fuse;
use git_on::{filesystem::Overlay, policy::WritePolicy, snapshot::Snapshot};
use std::ffi::OsStr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::thread::JoinHandle;
use tempfile::TempDir;

fn u32s(xs: &[u32]) -> Vec<u8> {
    xs.iter().flat_map(|x| x.to_ne_bytes()).collect()
}
fn u64s(xs: &[u64]) -> Vec<u8> {
    xs.iter().flat_map(|x| x.to_ne_bytes()).collect()
}
fn num(b: &[u8], offset: usize) -> u64 {
    u64::from_ne_bytes(b[offset..offset + 8].try_into().unwrap())
}
fn names(mut b: Vec<u8>, names: &[&str]) -> Vec<u8> {
    for n in names {
        b.extend_from_slice(n.as_bytes());
        b.push(0);
    }
    b
}
fn io(fh: u64, offset: i64, size: u32, flags: i32) -> Vec<u8> {
    [
        u64s(&[fh, offset as u64]),
        u32s(&[size, 0]),
        u64s(&[0]),
        u32s(&[flags as u32, 0]),
    ]
    .concat()
}
fn dirents(bytes: &[u8]) -> Vec<(u64, Vec<u8>)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        assert!(pos + 24 <= bytes.len());
        let len = u32::from_ne_bytes(bytes[pos + 16..pos + 20].try_into().unwrap()) as usize;
        let end = pos + ((24 + len + 7) & !7);
        assert!(end <= bytes.len());
        out.push((
            num(bytes, pos + 8),
            bytes[pos + 24..pos + 24 + len].to_vec(),
        ));
        pos = end;
    }
    out
}
struct Peer {
    fd: OwnedFd,
    task: Option<JoinHandle<()>>,
    unique: u64,
    uid: u32,
    gid: u32,
    _temp: TempDir,
}
impl Peer {
    fn new() -> Self {
        Self::with_policy(WritePolicy::Ignored)
    }
    fn with_policy(policy: WritePolicy) -> Self {
        Self::with_init(policy, false)
    }
    fn with_init(policy: WritePolicy, extended: bool) -> Self {
        Self::with_capabilities(policy, extended, 0).0
    }
    fn with_capabilities(policy: WritePolicy, extended: bool, extra: u32) -> (Self, u32) {
        Self::with_directory_size(policy, extended, extra, 0)
    }
    fn with_directory_size(
        policy: WritePolicy,
        extended: bool,
        extra: u32,
        count: usize,
    ) -> (Self, u32) {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::init(temp.path().join("repo")).unwrap();
        let mut tree = repo.treebuilder(None).unwrap();
        tree.insert(
            ".gitignore",
            repo.blob(b"*.tmp\n/cache/\n").unwrap(),
            0o100644,
        )
        .unwrap();
        tree.insert("tracked", repo.blob(b"original\n").unwrap(), 0o100644)
            .unwrap();
        tree.insert("link", repo.blob(b"tracked").unwrap(), 0o120000)
            .unwrap();
        if count != 0 {
            let content = repo.blob(b"unrequested payload").unwrap();
            for index in 0..count {
                tree.insert(format!("file-{index:05}"), content, 0o100644)
                    .unwrap();
            }
        }
        let oid = tree.write().unwrap();
        let snapshot =
            Snapshot::load(repo.path(), OsStr::new(&oid.to_string()), "protocol").unwrap();
        let overlay =
            Overlay::with_write_policy(snapshot, temp.path().join("upper"), policy).unwrap();
        let mut fds = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    fds.as_mut_ptr(),
                )
            },
            0
        );
        let fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let server = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        // Bound failures instead of hanging CI when a wire request is invalid.
        let timeout = libc::timeval {
            tv_sec: 5,
            tv_usec: 0,
        };
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVTIMEO,
                    (&timeout as *const libc::timeval).cast(),
                    std::mem::size_of_val(&timeout) as _,
                )
            },
            0
        );
        let task = std::thread::spawn(move || {
            Session::from_fd(
                Fuse::new(overlay),
                server,
                SessionACL::All,
                fuser::Config::default(),
            )
            .unwrap()
            .run()
            .unwrap();
        });
        let mut p = Self {
            fd,
            task: Some(task),
            unique: 0,
            // Tracked entries belong to the mounting process, including CI's
            // runner UID. Credential tests override these IDs explicitly.
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
            _temp: temp,
        };
        let minor = if extended { 40 } else { 31 };
        let mut body = vec![
            7,
            minor,
            0,
            (1 << 1) | (1 << 10) | (1 << 13) | (1 << 14) | (1 << 16) | extra,
        ];
        if extended {
            body[3] |= 1 << 30; // FUSE_INIT_EXT
            body.push(1 << 5); // FUSE_PASSTHROUGH high flag
            body.extend([0; 11]);
        }
        let init = p.ok(26, 0, u32s(&body));
        assert_eq!(&init[..4], &u32s(&[7]));
        let daemon_minor = u32::from_ne_bytes(init[4..8].try_into().unwrap());
        // FUSE negotiates MIN(kernel, daemon), not equality of advertisements.
        // Upstream fuser0.18 advertises7.40 while accepting a legacy7.31 peer.
        // The complete operation suite below still uses the peer's7.31 layout.
        // https://man7.org/linux/man-pages/man4/fuse.4.html (FUSE_INIT)
        assert_eq!(minor.min(daemon_minor), minor);
        // No POSIX locks, readdirplus or writeback-cache capabilities enabled.
        let flags = u32::from_ne_bytes(init[12..16].try_into().unwrap());
        assert_eq!(
            flags & ((1 << 1) | (1 << 10) | (1 << 13) | (1 << 14) | (1 << 16)),
            0
        );
        (p, flags)
    }
    fn send(&mut self, op: u32, ino: u64, body: Vec<u8>) {
        self.unique += 1;
        let packet = [
            u32s(&[(40 + body.len()) as u32, op]),
            u64s(&[self.unique, ino]),
            u32s(&[self.uid, self.gid, 1, 0]),
            body,
        ]
        .concat();
        assert_eq!(
            unsafe {
                libc::send(
                    self.fd.as_raw_fd(),
                    packet.as_ptr().cast(),
                    packet.len(),
                    libc::MSG_NOSIGNAL,
                )
            },
            packet.len() as isize,
            "send opcode {op}"
        );
    }
    fn call(&mut self, op: u32, ino: u64, body: Vec<u8>) -> Result<Vec<u8>, i32> {
        self.send(op, ino, body);
        let mut reply = vec![0u8; 131072];
        let n = unsafe {
            libc::recv(
                self.fd.as_raw_fd(),
                reply.as_mut_ptr().cast(),
                reply.len(),
                0,
            )
        };
        assert!(
            n >= 16,
            "no reply for opcode {op}: {}",
            std::io::Error::last_os_error()
        );
        reply.truncate(n as usize);
        assert_eq!(num(&reply, 8), self.unique);
        assert_eq!(
            u32::from_ne_bytes(reply[..4].try_into().unwrap()) as usize,
            reply.len()
        );
        let errno = i32::from_ne_bytes(reply[4..8].try_into().unwrap());
        if errno == 0 {
            Ok(reply[16..].to_vec())
        } else {
            Err(-errno)
        }
    }
    fn ok(&mut self, op: u32, ino: u64, body: Vec<u8>) -> Vec<u8> {
        self.call(op, ino, body)
            .unwrap_or_else(|e| panic!("opcode {op}, inode {ino}: errno {e}"))
    }
    fn err(&mut self, op: u32, ino: u64, body: Vec<u8>, e: i32) {
        assert_eq!(self.call(op, ino, body), Err(e), "opcode {op}, inode {ino}");
    }
    fn lookup(&mut self, parent: u64, name: &str) -> u64 {
        num(&self.ok(1, parent, names(vec![], &[name])), 0)
    }
    fn create(&mut self, name: &str) -> (u64, u64) {
        let b = self.ok(
            35,
            1,
            names(u32s(&[libc::O_RDWR as u32, 0o600, 0, 0]), &[name]),
        );
        (num(&b, 0), num(&b, 128))
    }
    fn open(&mut self, ino: u64, flags: i32) -> u64 {
        num(&self.ok(14, ino, u32s(&[flags as u32, 0])), 0)
    }
    fn release(&mut self, ino: u64, fh: u64) {
        self.ok(18, ino, [u64s(&[fh]), u32s(&[0, 0]), u64s(&[0])].concat());
    }
    fn destroy(&mut self) {
        // Upstream ends its event loop on DESTROY. Socket EOF is an invalid
        // request, unlike a real device's ENODEV after unmount. Require both
        // the protocol acknowledgement and successful session completion.
        self.ok(38, 0, vec![]);
        self.task.take().unwrap().join().unwrap();
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        if self.task.is_some() && !std::thread::panicking() {
            self.destroy();
        }
        unsafe {
            libc::shutdown(self.fd.as_raw_fd(), libc::SHUT_RDWR);
        }
    }
}

#[test]
fn protocol_short_and_extended_init_preserve_ordinary_fuse() {
    for extended in [false, true] {
        let mut peer = Peer::with_init(WritePolicy::Ignored, extended);
        peer.ok(3, 1, vec![0; 16]); // successful real dispatch after INIT
    }
}

#[test]
fn protocol_kernel_cache_only_retains_protected_source() {
    for policy in [
        WritePolicy::None,
        WritePolicy::Ignored,
        WritePolicy::Untracked,
        WritePolicy::All,
    ] {
        let mut p = Peer::with_policy(policy);
        let entry = p.ok(1, 1, names(vec![], &["tracked"]));
        let ino = num(&entry, 0);
        let protected = policy != WritePolicy::All;
        assert_eq!(num(&entry, 16) != 0, protected, "entry TTL: {policy:?}");
        assert_eq!(num(&entry, 24) != 0, protected, "attribute TTL: {policy:?}");
        let attributes = p.ok(3, ino, vec![0; 16]);
        assert_eq!(num(&attributes, 0) != 0, protected);
        // Protected source metadata stays pinned, including directory attrs.
        assert_eq!(num(&p.ok(3, 1, vec![0; 16]), 0) != 0, protected);
        let opened = p.ok(14, ino, u32s(&[libc::O_RDONLY as u32, 0]));
        let flags = u32::from_ne_bytes(opened[8..12].try_into().unwrap());
        assert_eq!(
            flags & ((1 << 1) | (1 << 5)),
            (1 << 1) | if protected { 1 << 5 } else { 0 }
        );
        p.release(ino, num(&opened, 0));
        if policy != WritePolicy::None {
            let (generated, handle) = p.create("generated.tmp");
            let entry = p.ok(1, 1, names(vec![], &["generated.tmp"]));
            assert_eq!(num(&entry, 16), 0);
            assert_eq!(num(&entry, 24), 0);
            let opened = p.ok(14, generated, u32s(&[libc::O_RDONLY as u32, 0]));
            let flags = u32::from_ne_bytes(opened[8..12].try_into().unwrap());
            assert_eq!(flags & ((1 << 1) | (1 << 5)), 0);
            p.release(generated, num(&opened, 0));
            p.release(generated, handle);
        }
    }
}

#[test]
fn protocol_cached_source_directory_attrs_do_not_cache_generated_membership() {
    for policy in [WritePolicy::Ignored, WritePolicy::Untracked] {
        let mut p = Peer::with_policy(policy);
        let before = p.ok(3, 1, vec![0; 16]);
        assert_ne!(num(&before, 0), 0, "protected root metadata is cacheable");
        let created = p.ok(9, 1, names(u32s(&[0o700, 0]), &["cache"]));
        let generated = num(&created, 0);
        assert_eq!(num(&p.ok(3, generated, vec![0; 16]), 0), 0);
        let after = p.ok(3, 1, vec![0; 16]);
        assert_eq!(&before[16..], &after[16..], "source metadata stays pinned");
        let handle = num(&p.ok(27, 1, u32s(&[0, 0])), 0);
        let listed = dirents(&p.ok(28, 1, io(handle, 0, 8192, 0)));
        assert!(listed.iter().any(|(_, name)| name == b"cache"));
        p.ok(29, 1, [u64s(&[handle]), u32s(&[0, 0]), u64s(&[0])].concat());
        p.ok(11, 1, names(vec![], &["cache"]));
        let handle = num(&p.ok(27, 1, u32s(&[0, 0])), 0);
        let listed = dirents(&p.ok(28, 1, io(handle, 0, 8192, 0)));
        assert!(!listed.iter().any(|(_, name)| name == b"cache"));
        p.ok(29, 1, [u64s(&[handle]), u32s(&[0, 0]), u64s(&[0])].concat());
    }
}

#[test]
fn protocol_all_source_data_cache_stops_after_copy_up_without_extending_ttls() {
    let mut p = Peer::with_policy(WritePolicy::All);
    let entry = p.ok(1, 1, names(vec![], &["tracked"]));
    let ino = num(&entry, 0);
    assert_eq!(num(&entry, 16), 0);
    assert_eq!(num(&entry, 24), 0);
    let opened = p.ok(14, ino, u32s(&[libc::O_RDONLY as u32, 0]));
    assert_eq!(
        u32::from_ne_bytes(opened[8..12].try_into().unwrap()) & ((1 << 1) | (1 << 5)),
        1 << 1
    );
    let reader = num(&opened, 0);
    assert_eq!(p.ok(15, ino, io(reader, 0, 100, 0)), b"original\n");
    let writer = p.open(ino, libc::O_RDWR);
    p.ok(
        16,
        ino,
        [io(writer, 0, 9, 0), b"changed!\n".to_vec()].concat(),
    );
    assert_eq!(p.ok(15, ino, io(reader, 0, 100, 0)), b"changed!\n");
    let reopened = p.ok(14, ino, u32s(&[libc::O_RDONLY as u32, 0]));
    assert_eq!(
        u32::from_ne_bytes(reopened[8..12].try_into().unwrap()) & ((1 << 1) | (1 << 5)),
        0
    );
    assert_eq!(num(&p.ok(3, ino, vec![0; 16]), 0), 0);
    p.release(ino, num(&reopened, 0));
    p.release(ino, writer);
    p.release(ino, reader);
}

#[test]
fn protocol_lifecycle_lookup_forget_and_batch_forget() {
    let mut p = Peer::new();
    let ino = p.lookup(1, "tracked");
    p.ok(3, ino, vec![0; 16]);
    p.err(1, 1, names(vec![], &["missing"]), libc::ENOENT);
    p.err(1, ino, names(vec![], &["child"]), libc::ENOTDIR);
    p.send(2, ino, u64s(&[1]));
    p.err(3, ino, vec![0; 16], libc::ENOENT);
    let a = p.lookup(1, "tracked");
    let b = p.lookup(1, "link");
    p.send(42, 0, [u32s(&[2, 0]), u64s(&[a, 1, b, 1])].concat());
    p.err(3, a, vec![0; 16], libc::ENOENT);
    p.err(3, b, vec![0; 16], libc::ENOENT);
    p.destroy(); // DESTROY is terminal; no requests follow its acknowledgement.
}

#[test]
fn protocol_file_io_sync_and_stale_handles() {
    let mut p = Peer::new();
    let tracked = p.lookup(1, "tracked");
    let rh = p.open(tracked, libc::O_RDONLY);
    assert_eq!(p.ok(15, tracked, io(rh, 0, 100, 0)), b"original\n");
    assert!(p.ok(15, tracked, io(rh, 99, 1, 0)).is_empty());
    p.err(15, tracked, io(rh, -1, 1, 0), libc::EINVAL);
    p.err(14, tracked, u32s(&[libc::O_WRONLY as u32, 0]), libc::EROFS);
    p.err(14, 1, u32s(&[0, 0]), libc::EISDIR);
    let link = p.lookup(1, "link");
    p.err(14, link, u32s(&[0, 0]), libc::ELOOP);
    assert_eq!(p.ok(5, link, vec![]), b"tracked");
    p.err(5, tracked, vec![], libc::EINVAL);
    let (ino, fh) = p.create("fresh.tmp");
    assert_eq!(
        p.ok(
            16,
            ino,
            [io(fh, 2, 3, libc::O_RDWR), b"abc".to_vec()].concat()
        ),
        u32s(&[3, 0])
    );
    assert_eq!(p.ok(15, ino, io(fh, 0, 100, 0)), b"\0\0abc");
    p.ok(25, ino, [u64s(&[fh]), u64s(&[0, 0])].concat());
    for datasync in [0, 1] {
        p.ok(20, ino, [u64s(&[fh]), u32s(&[datasync, 0])].concat());
        p.ok(20, tracked, [u64s(&[rh]), u32s(&[datasync, 0])].concat());
    }
    p.err(15, tracked, io(fh, 0, 1, 0), libc::EBADF);
    p.release(ino, fh);
    for op in [15, 16] {
        let body = if op == 16 {
            [io(fh, 0, 1, libc::O_RDWR), vec![1]].concat()
        } else {
            io(fh, 0, 1, 0)
        };
        p.err(op, ino, body, libc::EBADF);
    }
    p.err(25, ino, u64s(&[fh, 0, 0]), libc::EBADF);
    p.err(20, ino, [u64s(&[fh]), u32s(&[0, 0])].concat(), libc::EBADF);
    p.err(18, ino, u64s(&[fh, 0, 0]), libc::EBADF);
    p.err(3, ino, [u32s(&[1, 0]), u64s(&[fh])].concat(), libc::EBADF);
    let wh = p.open(ino, libc::O_WRONLY);
    p.err(15, ino, io(wh, 0, 1, 0), libc::EBADF);
    p.err(
        16,
        ino,
        [io(wh, -1, 1, libc::O_WRONLY), vec![1]].concat(),
        libc::EINVAL,
    );
}

#[test]
fn protocol_readonly_no_open_negotiation_and_stateless_reads() {
    const NO_OPEN: u32 = 1 << 17;
    for policy in [
        WritePolicy::Ignored,
        WritePolicy::Untracked,
        WritePolicy::All,
    ] {
        let (mut p, flags) = Peer::with_capabilities(policy, false, NO_OPEN);
        assert_eq!(flags & NO_OPEN, 0, "mutable namespaces retain real handles");
        let ino = p.lookup(1, "tracked");
        p.err(15, ino, io(0, 0, 100, 0), libc::EBADF);
        let handle = p.open(ino, libc::O_RDONLY);
        assert_ne!(handle, 0);
        assert_eq!(p.ok(15, ino, io(handle, 0, 100, 0)), b"original\n");
        p.release(ino, handle);
    }
    let (mut p, flags) = Peer::with_capabilities(WritePolicy::None, false, NO_OPEN);
    assert_ne!(flags & NO_OPEN, 0, "readonly peer must negotiate no-open");
    let ino = p.lookup(1, "tracked");
    p.err(14, ino, u32s(&[libc::O_RDONLY as u32, 0]), libc::ENOSYS);
    p.err(14, ino, u32s(&[libc::O_RDWR as u32, 0]), libc::EROFS);
    p.err(
        14,
        ino,
        u32s(&[(libc::O_RDONLY | libc::O_TRUNC) as u32, 0]),
        libc::EROFS,
    );
    p.err(14, 1, u32s(&[0, 0]), libc::EISDIR);
    let link = p.lookup(1, "link");
    p.err(14, link, u32s(&[0, 0]), libc::ELOOP);
    for _ in 0..3 {
        assert_eq!(p.ok(15, ino, io(0, 0, 4, 0)), b"orig");
        assert_eq!(p.ok(15, ino, io(0, 4, 100, 0)), b"inal\n");
        assert!(p.ok(15, ino, io(0, 100, 100, 0)).is_empty());
    }
    p.err(15, ino, io(0, -1, 1, 0), libc::EINVAL);
    p.err(15, ino, io(0, 0, 1, libc::O_WRONLY), libc::EBADF);
    p.err(15, ino, io(999, 0, 1, 0), libc::EBADF);
    let attr = p.ok(3, ino, [u32s(&[1, 0]), u64s(&[0])].concat());
    assert_eq!(num(&attr, 24), 9);
    for datasync in [0, 1] {
        p.ok(20, ino, [u64s(&[0]), u32s(&[datasync, 0])].concat());
    }
    // Linux memoizes ENOSYS for the entire readonly connection, so repeated
    // closes need neither flush nor release round trips after this response.
    p.err(25, ino, u64s(&[0, 0, 0]), libc::ENOSYS);
    p.err(15, 1, io(0, 0, 1, 0), libc::EISDIR);
    p.err(15, u64::MAX, io(0, 0, 1, 0), libc::ENOENT);
    p.send(2, ino, u64s(&[1]));
    p.err(15, ino, io(0, 0, 1, 0), libc::ENOENT);
    let reopened = p.lookup(1, "tracked");
    assert_eq!(p.ok(15, reopened, io(0, 0, 100, 0)), b"original\n");
}

#[test]
fn protocol_none_denies_supported_mutations_and_preserves_source() {
    let mut p = Peer::with_policy(WritePolicy::None);
    let tracked = p.lookup(1, "tracked");
    let read = p.open(tracked, libc::O_RDONLY);
    let before = p.ok(3, tracked, vec![0; 16]);
    p.err(14, tracked, u32s(&[libc::O_RDWR as u32, 0]), libc::EROFS);
    p.err(34, 1, u32s(&[libc::W_OK as u32, 0]), libc::EROFS);
    p.err(34, tracked, u32s(&[libc::W_OK as u32, 0]), libc::EROFS);
    p.err(
        35,
        1,
        names(u32s(&[libc::O_RDWR as u32, 0o600, 0, 0]), &["fresh.tmp"]),
        libc::EROFS,
    );
    p.err(
        8,
        1,
        names(u32s(&[libc::S_IFREG | 0o600, 0, 0, 0]), &["node.tmp"]),
        libc::EROFS,
    );
    p.err(9, 1, names(u32s(&[0o700, 0]), &["cache"]), libc::EROFS);
    p.err(6, 1, names(vec![], &["link.tmp", "tracked"]), libc::EROFS);
    p.err(13, 1, names(u64s(&[tracked]), &["alias.tmp"]), libc::EROFS);
    p.err(10, 1, names(vec![], &["tracked"]), libc::EROFS);
    p.err(
        12,
        1,
        names(u64s(&[1]), &["tracked", "renamed.tmp"]),
        libc::EROFS,
    );
    let mut setattr = vec![0; 88];
    setattr[..4].copy_from_slice(&1u32.to_ne_bytes());
    setattr[68..72].copy_from_slice(&0o600u32.to_ne_bytes());
    p.err(4, tracked, setattr, libc::EROFS);
    assert_eq!(p.ok(15, tracked, io(read, 0, 100, 0)), b"original\n");
    assert_eq!(p.ok(3, tracked, vec![0; 16]), before);
    for name in [
        "fresh.tmp",
        "node.tmp",
        "cache",
        "link.tmp",
        "alias.tmp",
        "renamed.tmp",
    ] {
        p.err(1, 1, names(vec![], &[name]), libc::ENOENT);
    }
}

#[test]
fn protocol_untracked_writes_unignored_paths_and_keeps_source_immutable() {
    let mut p = Peer::with_policy(WritePolicy::Untracked);
    let tracked = p.lookup(1, "tracked");
    let read = p.open(tracked, libc::O_RDONLY);
    let before = p.ok(3, tracked, vec![0; 16]);
    let (generated, handle) = p.create("not-ignored");
    p.ok(
        16,
        generated,
        [io(handle, 0, 7, libc::O_RDWR), b"private".to_vec()].concat(),
    );
    assert_eq!(p.ok(15, generated, io(handle, 0, 100, 0)), b"private");
    let directory = num(
        &p.ok(9, 1, names(u32s(&[0o700, 0]), &["generated-directory"])),
        0,
    );
    p.ok(
        6,
        directory,
        names(vec![], &["generated-link", "../tracked"]),
    );
    p.ok(
        13,
        directory,
        names(u64s(&[generated]), &["generated-alias"]),
    );
    p.ok(12, 1, names(u64s(&[1]), &["not-ignored", "renamed"]));
    p.err(
        12,
        1,
        names(u64s(&[1]), &["renamed", "tracked"]),
        libc::EROFS,
    );
    p.err(
        12,
        1,
        names(u64s(&[1]), &["tracked", "new-source-name"]),
        libc::EROFS,
    );
    p.err(10, 1, names(vec![], &["tracked"]), libc::EROFS);
    p.err(14, tracked, u32s(&[libc::O_RDWR as u32, 0]), libc::EROFS);
    p.err(34, tracked, u32s(&[libc::W_OK as u32, 0]), libc::EROFS);
    let mut setattr = vec![0; 88];
    setattr[..4].copy_from_slice(&1u32.to_ne_bytes());
    setattr[68..72].copy_from_slice(&0o600u32.to_ne_bytes());
    p.err(4, tracked, setattr, libc::EROFS);
    p.ok(10, 1, names(vec![], &["renamed"]));
    assert_eq!(p.ok(15, generated, io(handle, 0, 100, 0)), b"private");
    assert_eq!(p.ok(15, tracked, io(read, 0, 100, 0)), b"original\n");
    assert_eq!(p.ok(3, tracked, vec![0; 16]), before);
}

#[test]
fn protocol_all_copy_up_setattr_links_rename_and_whiteouts() {
    let mut p = Peer::with_policy(WritePolicy::All);
    let tracked = p.lookup(1, "tracked");
    let read = p.open(tracked, libc::O_RDONLY);
    p.ok(34, tracked, u32s(&[libc::W_OK as u32, 0]));
    let write = p.open(tracked, libc::O_RDWR);
    p.ok(
        16,
        tracked,
        [io(write, 0, 7, libc::O_RDWR), b"changed".to_vec()].concat(),
    );
    assert_eq!(p.ok(15, tracked, io(read, 0, 100, 0)), b"changedl\n");
    let mut setattr = vec![0; 88];
    setattr[..4].copy_from_slice(&(1u32 | 8).to_ne_bytes());
    setattr[16..24].copy_from_slice(&7u64.to_ne_bytes());
    setattr[68..72].copy_from_slice(&0o640u32.to_ne_bytes());
    p.ok(4, tracked, setattr);
    p.ok(13, 1, names(u64s(&[tracked]), &["alias"]));
    p.ok(12, 1, names(u64s(&[1]), &["tracked", "renamed"]));
    p.err(1, 1, names(vec![], &["tracked"]), libc::ENOENT);
    assert_eq!(p.lookup(1, "renamed"), tracked);
    assert_eq!(p.lookup(1, "alias"), tracked);
    assert_eq!(p.ok(15, tracked, io(read, 0, 100, 0)), b"changed");
    p.ok(10, 1, names(vec![], &["renamed"]));
    p.ok(10, 1, names(vec![], &["alias"]));
    assert_eq!(p.ok(15, tracked, io(read, 0, 100, 0)), b"changed");
    p.release(tracked, read);
    p.release(tracked, write);
    let (replacement, _) = p.create("tracked");
    assert_ne!(replacement, tracked);
    let link = p.lookup(1, "link");
    p.ok(12, 1, names(u64s(&[1]), &["link", "moved-link"]));
    assert_eq!(p.ok(5, link, vec![]), b"tracked");
    p.err(1, 1, names(vec![], &["link"]), libc::ENOENT);
    let mut setattr = vec![0; 88];
    setattr[..4].copy_from_slice(&1u32.to_ne_bytes());
    setattr[68..72].copy_from_slice(&0o700u32.to_ne_bytes());
    p.ok(4, 1, setattr);
    let directory = num(&p.ok(27, 1, u32s(&[0, 0])), 0);
    let entries = dirents(&p.ok(28, 1, io(directory, 0, 4096, 0)));
    assert_eq!(
        entries
            .iter()
            .filter(|(_, name)| name == b"tracked")
            .count(),
        1
    );
    assert!(!entries
        .iter()
        .any(|(_, name)| name == b"link" || name == b"alias" || name == b"renamed"));
}

#[test]
fn protocol_all_setattr_on_unlinked_original_read_handle() {
    let mut p = Peer::with_policy(WritePolicy::All);
    let tracked = p.lookup(1, "tracked");
    let read = p.open(tracked, libc::O_RDONLY);
    p.ok(10, 1, names(vec![], &["tracked"]));
    let mut setattr = vec![0; 88];
    setattr[..4].copy_from_slice(&(1u32 | 64).to_ne_bytes());
    setattr[8..16].copy_from_slice(&read.to_ne_bytes());
    setattr[68..72].copy_from_slice(&0o600u32.to_ne_bytes());
    p.ok(4, tracked, setattr);
    assert_eq!(p.ok(15, tracked, io(read, 0, 100, 0)), b"original\n");
    p.err(1, 1, names(vec![], &["tracked"]), libc::ENOENT);
    let attr = p.ok(3, tracked, vec![0; 16]);
    assert_eq!(u32::from_ne_bytes(attr[80..84].try_into().unwrap()), 0);
    p.release(tracked, read);
}

#[test]
fn protocol_creation_links_removal_and_rename_flags() {
    let mut p = Peer::new();
    let mknod = |name: &str, mode| names(u32s(&[mode, 0, 0o022, 0]), &[name]);
    let a = num(&p.ok(8, 1, mknod("node.tmp", libc::S_IFREG | 0o666)), 0);
    p.err(8, 1, mknod("node.tmp", libc::S_IFREG | 0o666), libc::EEXIST);
    p.err(8, 1, mknod("forbidden", libc::S_IFREG | 0o666), libc::EROFS);
    for typ in [libc::S_IFIFO, libc::S_IFSOCK, libc::S_IFCHR, libc::S_IFBLK] {
        p.err(8, 1, mknod("special.tmp", typ | 0o600), libc::EPERM);
    }
    p.err(1, 1, names(vec![], &["special.tmp"]), libc::ENOENT);
    let dir = num(&p.ok(9, 1, names(u32s(&[0o777, 0o022]), &["cache"])), 0);
    p.ok(6, dir, names(vec![], &["symlink", "../tracked"]));
    p.ok(13, dir, names(u64s(&[a]), &["alias"]));
    p.err(11, 1, names(vec![], &["cache"]), libc::ENOTEMPTY);
    let rename =
        |from: &str, to: &str, flags| names([u64s(&[1]), u32s(&[flags, 0])].concat(), &[from, to]);
    p.create("other.tmp");
    p.err(45, 1, rename("node.tmp", "other.tmp", 1), libc::EEXIST);
    for flags in [2, 4, 8] {
        p.err(45, 1, rename("node.tmp", "other.tmp", flags), libc::EINVAL);
    }
    assert_eq!(p.lookup(1, "node.tmp"), a);
    p.ok(45, 1, rename("node.tmp", "renamed.tmp", 1));
    p.ok(12, 1, names(u64s(&[1]), &["renamed.tmp", "node.tmp"]));
    p.err(10, 1, names(vec![], &["cache"]), libc::EISDIR);
    p.err(11, 1, names(vec![], &["node.tmp"]), libc::ENOTDIR);
    p.ok(10, dir, names(vec![], &["alias"]));
    p.ok(10, dir, names(vec![], &["symlink"]));
    p.ok(11, 1, names(vec![], &["cache"]));
    p.ok(10, 1, names(vec![], &["node.tmp"]));
    p.err(10, 1, names(vec![], &["tracked"]), libc::EROFS);
}

#[test]
fn protocol_partial_directory_pages_reopen_cookies_and_source_inode_identity() {
    for policy in [
        WritePolicy::None,
        WritePolicy::Ignored,
        WritePolicy::Untracked,
        WritePolicy::All,
    ] {
        let (mut p, _) = Peer::with_directory_size(policy, false, 0, 2000);
        let first = num(&p.ok(27, 1, u32s(&[0, 0])), 0);
        assert_eq!(
            dirents(&p.ok(28, 1, io(first, 0, 32, 0))),
            [(1, b".".to_vec())]
        );
        let second = num(&p.ok(27, 1, u32s(&[0, 0])), 0);
        let page = p.ok(28, 1, io(first, 100, 48, 0));
        assert_eq!(dirents(&page).len(), 1);
        assert_eq!(num(&page, 8), 101);
        assert_eq!(p.ok(28, 1, io(first, 100, 48, 0)), page);
        assert_eq!(p.ok(28, 1, io(second, 100, 48, 0)), page);
        let ino = num(&page, 0);
        let name = std::str::from_utf8(&dirents(&page)[0].1)
            .unwrap()
            .to_owned();
        p.ok(29, 1, u64s(&[first, 0, 0]));
        assert_eq!(
            p.lookup(1, &name),
            ino,
            "readdir and lookup identities differ"
        );
        p.send(2, ino, u64s(&[1]));
        p.ok(3, ino, vec![0; 16]);
        p.ok(29, 1, u64s(&[second, 0, 0]));
        p.err(3, ino, vec![0; 16], libc::ENOENT);
        assert_eq!(
            p.lookup(1, &name),
            ino,
            "same source changed identity after metadata eviction"
        );
        p.err(28, 1, io(first, 0, 48, 0), libc::EBADF);
    }
}

#[test]
fn protocol_source_directory_snapshot_survives_rename_unlink_and_newer_handle_close() {
    let mut p = Peer::with_policy(WritePolicy::All);
    let old = num(&p.ok(27, 1, u32s(&[0, 0])), 0);
    let before = p.ok(28, 1, io(old, 0, 4096, 0));
    let rows = dirents(&before);
    let tracked_offset = rows
        .iter()
        .position(|(_, name)| name == b"tracked")
        .unwrap();
    let tracked_ino = num(&p.ok(28, 1, io(old, tracked_offset as i64, 40, 0)), 0);
    p.ok(12, 1, names(u64s(&[1]), &["tracked", "renamed"]));
    let (late, late_fh) = p.create("late");
    p.release(late, late_fh);
    let latest = num(&p.ok(27, 1, u32s(&[0, 0])), 0);
    p.ok(29, 1, u64s(&[latest, 0, 0]));
    p.ok(10, 1, names(vec![], &["late"]));
    p.send(2, late, u64s(&[1]));
    p.err(3, late, vec![0; 16], libc::ENOENT);
    p.ok(10, 1, names(vec![], &["renamed"]));
    assert_eq!(p.ok(28, 1, io(old, 0, 4096, 0)), before);
    let reader = p.open(tracked_ino, libc::O_RDONLY);
    assert_eq!(p.ok(15, tracked_ino, io(reader, 0, 100, 0)), b"original\n");
    p.release(tracked_ino, reader);
    p.ok(29, 1, u64s(&[old, 0, 0]));
    p.err(3, tracked_ino, vec![0; 16], libc::ENOENT);
}

#[test]
fn protocol_directory_pagination_sync_statfs_and_bad_handles() {
    let mut p = Peer::new();
    let fh = num(&p.ok(27, 1, u32s(&[0, 0])), 0);
    let mut offset = 0;
    let mut names_seen = Vec::new();
    for page in 0..32 {
        let b = p.ok(28, 1, io(fh, offset, 40, 0));
        if b.is_empty() {
            break;
        }
        assert!(page < 31, "directory pagination did not terminate");
        for (next, name) in dirents(&b) {
            assert!(next > offset as u64, "directory cookie did not advance");
            offset = next as i64;
            names_seen.push(name);
        }
    }
    names_seen.sort();
    assert_eq!(
        names_seen,
        [".", "..", ".gitignore", "link", "tracked"].map(|s| s.as_bytes().to_vec())
    );
    p.err(28, 1, io(fh, -1, 40, 0), libc::EINVAL);
    for datasync in [0, 1] {
        p.ok(30, 1, [u64s(&[fh]), u32s(&[datasync, 0])].concat());
    }
    let stats = p.ok(17, 1, vec![]);
    assert!(num(&stats, 0) > 0);
    assert!(num(&stats, 16) <= num(&stats, 8));
    p.ok(29, 1, u64s(&[fh, 0, 0]));
    p.err(28, 1, io(fh, 0, 40, 0), libc::EBADF);
    p.err(29, 1, u64s(&[fh, 0, 0]), libc::EBADF);
    p.err(30, 1, [u64s(&[fh]), u32s(&[0, 0])].concat(), libc::EBADF);
    let tracked = p.lookup(1, "tracked");
    p.err(27, tracked, u32s(&[0, 0]), libc::ENOTDIR);
}

#[test]
fn protocol_access_and_setattr_credentials_and_boundaries() {
    // Exercise an ordinary owner even when the test process itself is root.
    access_and_setattr_credentials_and_boundaries(unsafe { libc::geteuid() }.max(1), unsafe {
        libc::getegid()
    });
}

#[test]
fn protocol_access_and_setattr_distinct_uid_and_gid() {
    // Synthetic wire credentials are independent of the host process IDs.
    // The second pair makes stranger_gid equal owner_uid: mixing the UID/GID
    // payloads would incorrectly turn the UID rejection into a permitted no-op.
    for (owner_uid, owner_gid) in [(1000, 1001), (1, 0)] {
        access_and_setattr_credentials_and_boundaries(owner_uid, owner_gid);
    }
}

fn access_and_setattr_credentials_and_boundaries(owner_uid: u32, owner_gid: u32) {
    let mut p = Peer::new();
    assert_ne!(owner_uid, 0);
    let stranger_uid = if owner_uid == 1 { 2 } else { 1 };
    let stranger_gid = if owner_gid == 0 { 1 } else { 0 };
    p.uid = owner_uid;
    p.gid = owner_gid;
    let (ino, fh) = p.create("mode.tmp");
    let access = |mask| u32s(&[mask, 0]);
    p.ok(34, ino, access((libc::R_OK | libc::W_OK) as u32));
    p.err(34, ino, access(libc::X_OK as u32), libc::EACCES);
    p.uid = stranger_uid;
    p.err(34, ino, access(libc::R_OK as u32), libc::EACCES);
    let mut setattr = vec![0; 88];
    setattr[..4].copy_from_slice(&1u32.to_ne_bytes());
    setattr[68..72].copy_from_slice(&0o640u32.to_ne_bytes());
    p.err(4, ino, setattr.clone(), libc::EPERM);
    p.uid = owner_uid;
    p.ok(4, ino, setattr);
    p.uid = stranger_uid;
    p.ok(34, ino, access(libc::R_OK as u32));
    p.gid = stranger_gid;
    p.err(34, ino, access(libc::R_OK as u32), libc::EACCES);
    p.uid = 0;
    p.ok(34, ino, access(libc::R_OK as u32));
    p.err(34, ino, access(libc::X_OK as u32), libc::EACCES);
    let tracked = p.lookup(1, "tracked");
    p.err(34, tracked, access(libc::W_OK as u32), libc::EROFS);
    p.ok(34, 1, access(libc::X_OK as u32));
    p.err(34, u64::MAX, access(0), libc::ENOENT);
    p.uid = owner_uid;
    p.gid = owner_gid;
    let mut setattr = vec![0; 88];
    setattr[..4].copy_from_slice(&(2u32).to_ne_bytes());
    setattr[76..80].copy_from_slice(&stranger_uid.to_ne_bytes());
    p.err(4, ino, setattr, libc::EPERM);
    let mut setattr = vec![0; 88];
    setattr[..4].copy_from_slice(&4u32.to_ne_bytes());
    setattr[80..84].copy_from_slice(&stranger_gid.to_ne_bytes());
    p.err(4, ino, setattr, libc::EPERM);
    for caller_uid in [owner_uid, 0] {
        p.uid = caller_uid;
        for (valid, offset, owner_id, stranger_id) in [
            (2u32, 76, owner_uid, stranger_uid),
            (4u32, 80, owner_gid, stranger_gid),
        ] {
            let mut setattr = vec![0; 88];
            setattr[..4].copy_from_slice(&valid.to_ne_bytes());
            setattr[offset..offset + 4].copy_from_slice(&owner_id.to_ne_bytes());
            p.ok(4, ino, setattr.clone());
            let before = p.ok(3, ino, vec![0; 16]);
            assert_eq!(
                u32::from_ne_bytes(before[84..88].try_into().unwrap()),
                owner_uid
            );
            assert_eq!(
                u32::from_ne_bytes(before[88..92].try_into().unwrap()),
                owner_gid
            );
            setattr[offset..offset + 4].copy_from_slice(&stranger_id.to_ne_bytes());
            p.err(4, ino, setattr, libc::EPERM);
            assert_eq!(p.ok(3, ino, vec![0; 16]), before);
        }
    }
    p.uid = owner_uid;
    let mut setattr = vec![0; 88];
    setattr[..4].copy_from_slice(&(8u32 | 64).to_ne_bytes());
    setattr[8..16].copy_from_slice(&fh.to_ne_bytes());
    setattr[16..24].copy_from_slice(&3u64.to_ne_bytes());
    p.ok(4, ino, setattr.clone());
    assert_eq!(p.ok(15, ino, io(fh, 0, 10, 0)), vec![0; 3]);
    let ro = p.open(ino, libc::O_RDONLY);
    setattr[8..16].copy_from_slice(&ro.to_ne_bytes());
    p.err(4, ino, setattr.clone(), libc::EBADF);
    p.err(4, tracked, setattr, libc::EROFS);
}

#[test]
fn protocol_all_unsupported_callbacks_refuse_without_mutation() {
    let mut p = Peer::new();
    let (ino, fh) = p.create("optional.tmp");
    p.ok(
        16,
        ino,
        [io(fh, 0, 8, libc::O_RDWR), b"sentinel".to_vec()].concat(),
    );
    let before = p.ok(3, ino, vec![0; 16]);
    let directory = num(&p.ok(27, 1, u32s(&[0, 0])), 0);
    let cases = vec![
        (
            21,
            names(u32s(&[1, 0]), &["user.test"])
                .into_iter()
                .chain([42])
                .collect(),
        ),
        (22, names(u32s(&[0, 0]), &["user.test"])),
        (23, u32s(&[0, 0])),
        (24, names(vec![], &["user.test"])),
        (
            31,
            [u64s(&[fh, 0, 0, 1]), u32s(&[libc::F_RDLCK as u32, 1, 0, 0])].concat(),
        ),
        (
            32,
            [u64s(&[fh, 0, 0, 1]), u32s(&[libc::F_WRLCK as u32, 1, 0, 0])].concat(),
        ),
        (
            33,
            [u64s(&[fh, 0, 0, 1]), u32s(&[libc::F_WRLCK as u32, 1, 0, 0])].concat(),
        ),
        (37, [u64s(&[0]), u32s(&[4096, 0])].concat()),
        (
            39,
            [u64s(&[fh]), u32s(&[0, 0]), u64s(&[0]), u32s(&[0, 0])].concat(),
        ),
        (
            40,
            [u64s(&[fh, 0]), u32s(&[0, libc::POLLIN as u32])].concat(),
        ),
        (43, [u64s(&[fh, 0, 4096]), u32s(&[0, 0])].concat()),
        (
            46,
            [u64s(&[fh, 0]), u32s(&[libc::SEEK_DATA as u32, 0])].concat(),
        ),
        (47, u64s(&[fh, 0, ino, fh, 0, 1, 0])),
    ];
    for (op, body) in cases {
        p.err(op, ino, body, libc::ENOSYS);
    }
    p.err(44, 1, io(directory, 0, 4096, 0), libc::ENOSYS);
    assert_eq!(p.ok(15, ino, io(fh, 0, 100, 0)), b"sentinel");
    assert_eq!(p.lookup(1, "optional.tmp"), ino);
    assert_eq!(p.ok(3, ino, vec![0; 16]), before);
}

#[test]
fn protocol_namespace_errors_and_atomic_rejections() {
    let mut p = Peer::new();
    for name in ["..", ".", "a/b", ""] {
        p.err(1, 1, names(vec![], &[name]), libc::EINVAL);
    }
    p.err(1, 1, names(vec![], &[&"a".repeat(256)]), libc::ENAMETOOLONG);
    p.err(1, u64::MAX, names(vec![], &["x"]), libc::ENOENT);
    p.err(10, 1, names(vec![], &["missing.tmp"]), libc::ENOENT);
    p.err(11, 1, names(vec![], &["missing.tmp"]), libc::ENOENT);
    let dir = num(&p.ok(9, 1, names(u32s(&[0o777, 0o027]), &["cache"])), 0);
    let attr = p.ok(3, dir, vec![0; 16]);
    assert_eq!(
        u32::from_ne_bytes(attr[76..80].try_into().unwrap()) & 0o777,
        0o750
    );
    p.err(9, 1, names(u32s(&[0o777, 0]), &["cache"]), libc::EEXIST);
    p.err(9, 1, names(u32s(&[0o777, 0]), &["forbidden"]), libc::EROFS);
    p.err(13, 1, names(u64s(&[dir]), &["directory.tmp"]), libc::EPERM);
    let (a, fh) = p.create("a.tmp");
    p.ok(
        16,
        a,
        [io(fh, 0, 3, libc::O_RDWR), b"abc".to_vec()].concat(),
    );
    p.ok(13, 1, names(u64s(&[a]), &["alias.tmp"]));
    let rename =
        |from: &str, to: &str, flags| names([u64s(&[1]), u32s(&[flags, 0])].concat(), &[from, to]);
    p.ok(45, 1, rename("a.tmp", "a.tmp", 0));
    p.err(45, 1, rename("a.tmp", "a.tmp", 1), libc::EEXIST);
    p.ok(45, 1, rename("a.tmp", "alias.tmp", 0));
    assert_eq!(p.lookup(1, "a.tmp"), a);
    assert_eq!(p.lookup(1, "alias.tmp"), a);
    p.ok(9, 1, names(u32s(&[0o700, 0]), &["directory.tmp"]));
    p.err(45, 1, rename("a.tmp", "directory.tmp", 0), libc::EISDIR);
    p.err(45, 1, rename("directory.tmp", "a.tmp", 0), libc::ENOTDIR);
    p.err(45, 1, rename("a.tmp", "forbidden", 0), libc::EROFS);
    assert_eq!(p.ok(15, a, io(fh, 0, 3, 0)), b"abc");
    p.ok(10, 1, names(vec![], &["a.tmp"]));
    p.ok(10, 1, names(vec![], &["alias.tmp"]));
    p.err(13, 1, names(u64s(&[a]), &["resurrect.tmp"]), libc::ENOENT);
    assert_eq!(p.ok(15, a, io(fh, 0, 3, 0)), b"abc");
    p.release(a, fh);
    p.send(2, a, u64s(&[u64::MAX]));
    p.err(3, a, vec![0; 16], libc::ENOENT);
    // Unknown forget IDs and excessive counts must not corrupt the root.
    p.send(2, u64::MAX, u64s(&[u64::MAX]));
    p.ok(3, 1, vec![0; 16]);
}

#[test]
fn protocol_zero_length_sync_writes_and_setattr_times() {
    let mut p = Peer::new();
    let (ino, fh) = p.create("bounds.tmp");
    assert_eq!(p.ok(16, ino, io(fh, 0, 0, libc::O_RDWR)), u32s(&[0, 0]));
    for flags in [libc::O_RDWR | libc::O_SYNC, libc::O_RDWR | libc::O_DSYNC] {
        assert_eq!(
            p.ok(16, ino, [io(fh, 0, 1, flags), vec![42]].concat()),
            u32s(&[1, 0])
        );
    }
    p.err(
        16,
        ino,
        [io(fh, i64::MAX, 1, libc::O_RDWR), vec![1]].concat(),
        libc::EFBIG,
    );
    let ro = p.open(ino, libc::O_RDONLY);
    p.err(16, ino, [io(ro, 0, 1, 0), vec![1]].concat(), libc::EBADF);
    let mut sa = vec![0; 88];
    sa[..4].copy_from_slice(&8u32.to_ne_bytes());
    sa[16..24].copy_from_slice(&u64::MAX.to_ne_bytes());
    p.err(4, ino, sa, libc::EFBIG);
    assert_eq!(p.ok(15, ino, io(fh, 0, 10, 0)), vec![42]);
    let mut sa = vec![0; 88];
    sa[..4].copy_from_slice(&(16u32 | 32 | 1024).to_ne_bytes());
    sa[32..40].copy_from_slice(&100u64.to_ne_bytes());
    sa[40..48].copy_from_slice(&200u64.to_ne_bytes());
    sa[48..56].copy_from_slice(&300u64.to_ne_bytes());
    let attr = p.ok(4, ino, sa);
    assert_eq!(num(&attr, 40), 100);
    assert_eq!(num(&attr, 48), 200);
    assert_eq!(num(&attr, 56), 300);
    let mut sa = vec![0; 88];
    sa[..4].copy_from_slice(&(16u32 | 32 | 128 | 256).to_ne_bytes());
    let attr = p.ok(4, ino, sa);
    assert!(num(&attr, 40) > 100);
    assert!(num(&attr, 48) > 200);
    // Multiple flushes do not close the open description; RELEASE does.
    for _ in 0..3 {
        p.ok(25, ino, u64s(&[fh, 0, 0]));
    }
    assert_eq!(p.ok(15, ino, io(fh, 0, 10, 0)), vec![42]);
}

#[test]
fn protocol_backing_failures_propagate_without_namespace_insertion() {
    let mut p = Peer::new();
    let backing = p._temp.path().join("upper");
    let moved = p._temp.path().join("temporarily-moved");
    std::fs::rename(&backing, &moved).unwrap();
    p.err(17, 1, vec![], libc::ENOENT);
    p.err(
        8,
        1,
        names(u32s(&[libc::S_IFREG | 0o600, 0, 0, 0]), &["failed.tmp"]),
        libc::ENOENT,
    );
    p.err(
        35,
        1,
        names(u32s(&[libc::O_RDWR as u32, 0o600, 0, 0]), &["failed.tmp"]),
        libc::ENOENT,
    );
    p.err(1, 1, names(vec![], &["failed.tmp"]), libc::ENOENT);
    let dir = num(&p.ok(27, 1, u32s(&[0, 0])), 0);
    p.err(30, 1, [u64s(&[dir]), u32s(&[0, 0])].concat(), libc::ENOENT);
    std::fs::rename(&moved, &backing).unwrap();
    p.ok(17, 1, vec![]);
    p.create("failed.tmp");
}

#[test]
fn protocol_create_metadata_stale_setattr_and_directory_stream_lifetime() {
    let mut p = Peer::new();
    let created = p.ok(
        35,
        1,
        names(
            u32s(&[libc::O_RDWR as u32, 0o666, 0o027, 0]),
            &["metadata.tmp"],
        ),
    );
    let ino = num(&created, 0);
    let fh = num(&created, 128);
    let attr = p.ok(3, ino, vec![0; 16]);
    assert_eq!(
        u32::from_ne_bytes(attr[76..80].try_into().unwrap()) & 0o777,
        0o640
    );
    assert_eq!(u32::from_ne_bytes(attr[84..88].try_into().unwrap()), p.uid);
    assert_eq!(u32::from_ne_bytes(attr[88..92].try_into().unwrap()), p.gid);
    let mut sa = vec![0; 88];
    sa[..4].copy_from_slice(&4u32.to_ne_bytes());
    let different_gid: u32 = if p.gid == 0 { 1 } else { 0 };
    sa[80..84].copy_from_slice(&different_gid.to_ne_bytes());
    p.err(4, ino, sa, libc::EPERM);
    let mut sa = vec![0; 88];
    sa[..4].copy_from_slice(&(8u32 | 64).to_ne_bytes());
    sa[8..16].copy_from_slice(&fh.to_ne_bytes());
    let (other, _) = p.create("other.tmp");
    p.err(4, other, sa.clone(), libc::EBADF);
    p.release(ino, fh);
    p.err(4, ino, sa, libc::EBADF);
    let dir = num(&p.ok(9, 1, names(u32s(&[0o700, 0]), &["cache"])), 0);
    p.ok(
        8,
        dir,
        names(u32s(&[libc::S_IFREG | 0o600, 0, 0, 0]), &["old"]),
    );
    let dh = num(&p.ok(27, dir, u32s(&[0, 0])), 0);
    let first = p.ok(28, dir, io(dh, 0, 32, 0));
    assert!(!first.is_empty());
    p.ok(10, dir, names(vec![], &["old"]));
    p.ok(
        8,
        dir,
        names(u32s(&[libc::S_IFREG | 0o600, 0, 0, 0]), &["new"]),
    );
    // A directory handle retains its enumeration snapshot and stable cookies.
    let listing = p.ok(28, dir, io(dh, 0, 4096, 0));
    assert_eq!(
        dirents(&listing)
            .into_iter()
            .map(|(_, name)| name)
            .collect::<Vec<_>>(),
        [b".".to_vec(), b"..".to_vec(), b"old".to_vec()]
    );
    let newdh = num(&p.ok(27, dir, u32s(&[0, 0])), 0);
    let listing = p.ok(28, dir, io(newdh, 0, 4096, 0));
    assert_eq!(
        dirents(&listing)
            .into_iter()
            .map(|(_, name)| name)
            .collect::<Vec<_>>(),
        [b".".to_vec(), b"..".to_vec(), b"new".to_vec()]
    );
    p.ok(10, dir, names(vec![], &["new"]));
    p.ok(11, 1, names(vec![], &["cache"]));
    p.ok(28, dir, io(dh, 0, 4096, 0));
    p.ok(29, dir, u64s(&[dh, 0, 0]));
    p.ok(29, dir, u64s(&[newdh, 0, 0]));
    p.send(2, dir, u64s(&[u64::MAX]));
    p.err(3, dir, vec![0; 16], libc::ENOENT);
}

#[test]
fn protocol_partial_backing_write_preserves_visible_progress() {
    // RLIMIT_FSIZE and SIGXFSZ are process-wide. Isolate this regression from
    // parallel test threads instead of changing their limits or dispositions.
    const CHILD: &str = "GIT_ON_PARTIAL_WRITE_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "protocol_partial_backing_write_preserves_visible_progress",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let mut p = Peer::new();
    let (ino, fh) = p.create("partial.tmp");
    let mut original: libc::rlimit = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, &mut original) },
        0
    );
    let limit = libc::rlimit {
        rlim_cur: 3,
        rlim_max: original.rlim_max,
    };
    let previous = unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) };
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &limit) }, 0);
    let result = p.call(
        16,
        ino,
        [io(fh, 0, 6, libc::O_RDWR), b"abcdef".to_vec()].concat(),
    );
    // A subsequent no-progress attempt reports the original capacity failure.
    let failure = p.call(16, ino, [io(fh, 3, 1, libc::O_RDWR), vec![42]].concat());
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &original) }, 0);
    unsafe {
        libc::signal(libc::SIGXFSZ, previous);
    }
    assert_eq!(result, Ok(u32s(&[3, 0])));
    assert_eq!(failure, Err(libc::EFBIG));
    let attr = p.ok(3, ino, vec![0; 16]);
    assert_eq!(num(&attr, 24), 3);
    assert_eq!(
        std::fs::read(p._temp.path().join("upper").join(ino.to_string())).unwrap(),
        b"abc"
    );
    assert_eq!(p.ok(15, ino, io(fh, 0, 100, 0)), b"abc");
}

#[test]
fn protocol_sparse_allocation_matches_backing_after_truncate_and_write() {
    use std::os::unix::fs::MetadataExt;
    let mut p = Peer::new();
    let (ino, fh) = p.create("sparse.tmp");
    for size in [1 << 30, 0] {
        let mut sa = vec![0; 88];
        sa[..4].copy_from_slice(&8u32.to_ne_bytes());
        sa[16..24].copy_from_slice(&(size as u64).to_ne_bytes());
        let attr = p.ok(4, ino, sa);
        let metadata =
            std::fs::metadata(p._temp.path().join("upper").join(ino.to_string())).unwrap();
        assert_eq!(num(&attr, 24), metadata.len());
        assert_eq!(num(&attr, 32), metadata.blocks());
    }
    p.ok(
        16,
        ino,
        [io(fh, 1 << 20, 1, libc::O_RDWR), vec![42]].concat(),
    );
    let attr = p.ok(3, ino, vec![0; 16]);
    let metadata = std::fs::metadata(p._temp.path().join("upper").join(ino.to_string())).unwrap();
    assert_eq!(num(&attr, 24), metadata.len());
    assert_eq!(num(&attr, 32), metadata.blocks());
    assert_eq!(p.ok(15, ino, io(fh, (1 << 20) - 1, 2, 0)), vec![0, 42]);
}

#[test]
fn protocol_mutable_directory_links_follow_namespace_changes() {
    let mut p = Peer::new();
    let mkdir =
        |p: &mut Peer, parent, name| num(&p.ok(9, parent, names(u32s(&[0o755, 0]), &[name])), 0);
    let links = |p: &mut Peer, ino| {
        let attr = p.ok(3, ino, vec![0; 16]);
        u32::from_ne_bytes(attr[80..84].try_into().unwrap())
    };
    let root_before = links(&mut p, 1);
    let cache = mkdir(&mut p, 1, "cache");
    let left = mkdir(&mut p, cache, "left");
    let right = mkdir(&mut p, cache, "right");
    assert_eq!(links(&mut p, cache), 4);
    assert_eq!(links(&mut p, 1), root_before);
    let child = mkdir(&mut p, left, "child");
    assert_eq!(links(&mut p, left), 3);
    p.ok(12, left, names(u64s(&[right]), &["child", "child"]));
    assert_eq!(links(&mut p, left), 2);
    assert_eq!(links(&mut p, right), 3);
    mkdir(&mut p, left, "replacement");
    p.ok(12, right, names(u64s(&[left]), &["child", "replacement"]));
    assert_eq!(links(&mut p, left), 3);
    assert_eq!(links(&mut p, right), 2);
    p.ok(12, left, names(u64s(&[left]), &["replacement", "renamed"]));
    assert_eq!(links(&mut p, left), 3);
    p.ok(11, left, names(vec![], &["renamed"]));
    assert_eq!(links(&mut p, left), 2);
    assert_eq!(links(&mut p, child), 0);
}
