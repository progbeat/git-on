//! Actual macOS fuser dispatcher over a private datagram peer, not a mount.
#![cfg(target_os = "macos")]
use fuser::{Filesystem, ReplyEmpty, Request, Session, SessionACL};
use std::{
    ffi::OsStr,
    os::fd::{AsRawFd, OwnedFd},
    os::unix::net::UnixDatagram,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

struct RenameRecorder(Arc<AtomicU32>);
impl Filesystem for RenameRecorder {
    fn rename(
        &mut self,
        _req: &Request<'_>,
        _parent: u64,
        _name: &OsStr,
        _newparent: u64,
        _newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        self.0.store(flags, Ordering::SeqCst);
        reply.ok();
    }
}

#[test]
fn macos_exclusive_rename_flags_survive_actual_dispatch() {
    let (client, server) = UnixDatagram::pair().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let stopper = server.try_clone().unwrap();
    let flags = Arc::new(AtomicU32::new(u32::MAX));
    let recorded = flags.clone();
    let worker = std::thread::spawn(move || {
        Session::from_fd(
            RenameRecorder(recorded),
            OwnedFd::from(server),
            SessionACL::Owner,
        )
        .run()
        .unwrap();
    });
    let call = |opcode: u32, unique: u64, payload: Vec<u8>| {
        let mut request = Vec::new();
        request.extend_from_slice(&(40u32 + payload.len() as u32).to_ne_bytes());
        request.extend_from_slice(&opcode.to_ne_bytes());
        request.extend_from_slice(&unique.to_ne_bytes());
        request.extend_from_slice(&1u64.to_ne_bytes());
        for value in [
            unsafe { libc::geteuid() },
            unsafe { libc::getegid() },
            std::process::id(),
            0,
        ] {
            request.extend_from_slice(&value.to_ne_bytes());
        }
        request.extend(payload);
        client.send(&request).unwrap();
        let mut reply = [0u8; 4096];
        let count = client.recv(&mut reply).unwrap();
        assert!(count >= 16);
        assert_eq!(i32::from_ne_bytes(reply[4..8].try_into().unwrap()), 0);
    };
    call(
        26,
        1,
        [7u32, 19, 0, 0]
            .into_iter()
            .flat_map(u32::to_ne_bytes)
            .collect(),
    );
    // macFUSE FUSE_RENAME_EXCL is 1<<26; the public Filesystem callback uses
    // Linux's portable no-replace flag 1. Previously the dispatcher lost it.
    let mut rename = 1u64.to_ne_bytes().to_vec();
    rename.extend((1u32 << 26).to_ne_bytes());
    rename.extend(0u32.to_ne_bytes());
    rename.extend_from_slice(b"from\0to\0");
    call(12, 2, rename);
    let observed = flags.load(Ordering::SeqCst);
    unsafe {
        libc::shutdown(stopper.as_raw_fd(), libc::SHUT_RDWR);
    }
    worker.join().unwrap();
    assert_eq!(
        observed, 1,
        "macOS exclusive rename must not turn into overwrite"
    );
}
