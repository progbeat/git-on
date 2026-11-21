use super::{
    protocol::{StateId, MAX_OPS},
    read_record,
    server::Server,
    status::*,
    xdr::{fragment, Reader, Writer, MAX_RECORD},
    FilesystemFailures,
};
use crate::{filesystem::Overlay, policy::WritePolicy, snapshot::Snapshot};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;

pub(super) fn fixture(policy: WritePolicy) -> (tempfile::TempDir, Server) {
    fixture_with_content(policy, b"tracked bytes")
}

fn fixture_with_content(policy: WritePolicy, content: &[u8]) -> (tempfile::TempDir, Server) {
    let directory = tempfile::tempdir().unwrap();
    let repo = git2::Repository::init(directory.path()).unwrap();
    let blob = repo.blob(content).unwrap();
    let mut tree = repo.treebuilder(None).unwrap();
    tree.insert("tracked", blob, 0o100644).unwrap();
    let tree = tree.write().unwrap().to_string();
    let id = uuid::Uuid::new_v4();
    let snapshot = Snapshot::load(directory.path(), tree.as_ref(), &id.to_string()).unwrap();
    let core =
        Overlay::with_write_policy(snapshot, directory.path().join("backing"), policy).unwrap();
    (
        directory,
        Server::new(core, id, Arc::new(FilesystemFailures::default())),
    )
}

pub(super) fn xdr(write: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut w = Writer::default();
    write(&mut w);
    w.into_bytes()
}

fn call(xid: u32, count: u32, ops: &[u8]) -> Vec<u8> {
    call_as(
        xid,
        count,
        ops,
        unsafe { libc::geteuid() },
        unsafe { libc::getegid() },
        &[],
    )
}

pub(super) fn call_as(
    xid: u32,
    count: u32,
    ops: &[u8],
    uid: u32,
    gid: u32,
    groups: &[u32],
) -> Vec<u8> {
    xdr(|w| {
        for n in [xid, 0, 2, 100003, 4, 1, 1] {
            w.u32(n);
        }
        let auth = xdr(|w| {
            w.u32(0);
            w.opaque(b"git-on-test");
            w.u32(uid);
            w.u32(gid);
            w.u32(groups.len() as u32);
            for &group in groups {
                w.u32(group);
            }
        });
        w.opaque(&auth);
        w.u32(0);
        w.u32(0);
        w.opaque(b"test");
        w.u32(1);
        w.u32(count);
        w.fixed(ops);
    })
}

pub(super) fn results(reply: &[u8], expected: u32, count: u32) -> Reader<'_> {
    let mut r = Reader::new(reply);
    r.take(20).unwrap();
    assert_eq!(r.u32().unwrap(), 0); // RPC accepted
    assert_eq!(r.u32().unwrap(), expected);
    assert_eq!(r.opaque(1024).unwrap(), b"test");
    assert_eq!(r.u32().unwrap(), count);
    r
}

fn session(server: &mut Server) -> [u8; 16] {
    session_with(server, call)
}

pub(super) fn session_with(
    server: &mut Server,
    call: impl Fn(u32, u32, &[u8]) -> Vec<u8>,
) -> [u8; 16] {
    let create = create_session_request(server, &call, MAX_RECORD as u32);
    let reply = server.rpc(&call(2, 1, &create)).unwrap();
    let mut r = results(&reply, OK, 1);
    assert_eq!((r.u32().unwrap(), r.u32().unwrap()), (43, OK));
    r.fixed().unwrap()
}

fn create_session_request(
    server: &mut Server,
    call: impl Fn(u32, u32, &[u8]) -> Vec<u8>,
    reply_limit: u32,
) -> Vec<u8> {
    let exchange = xdr(|w| {
        w.u32(42);
        w.fixed(&[7; 8]);
        w.opaque(b"protocol test");
        w.u32(0x10000);
        w.u32(0);
        w.u32(0);
    });
    let reply = server.rpc(&call(1, 1, &exchange)).unwrap();
    let mut r = results(&reply, OK, 1);
    assert_eq!((r.u32().unwrap(), r.u32().unwrap()), (42, OK));
    let client = r.u64().unwrap();
    let sequence = r.u32().unwrap();
    xdr(|w| {
        w.u32(43);
        w.u64(client);
        w.u32(sequence);
        w.u32(0);
        for _ in 0..2 {
            for n in [
                0,
                MAX_RECORD as u32,
                reply_limit,
                reply_limit,
                MAX_OPS as u32,
                4,
                0,
            ] {
                w.u32(n);
            }
        }
        w.u32(0);
        w.u32(0);
    })
}

#[test]
fn create_session_replay_keeps_only_the_operation_payload() {
    let (_dir, mut server) = fixture(WritePolicy::None);
    let request = create_session_request(&mut server, call, MAX_RECORD as u32);
    let original = server.rpc(&call(2, 1, &request)).unwrap();
    let replay = server.rpc(&call(3, 1, &request)).unwrap();
    assert_eq!(original[4..], replay[4..]);
}

#[test]
fn cached_read_reply_fits_the_negotiated_slot_budget() {
    const COUNT: usize = 600 * 1024;
    const LIMIT: usize = 1024 * 1024;
    let (_dir, mut server) = fixture_with_content(WritePolicy::None, &vec![b'x'; COUNT]);
    let create = create_session_request(&mut server, call, LIMIT as u32);
    let reply = server.rpc(&call(2, 1, &create)).unwrap();
    let mut response = results(&reply, OK, 1);
    stat(&mut response, 43, OK);
    let session = response.fixed().unwrap();
    let ops = xdr(|w| {
        sequence(w, &session, 1, 0, 0);
        lookup_tracked(w);
        w.u32(25); // READ
        StateId::ANONYMOUS.encode(w);
        w.u64(0);
        w.u32(COUNT as u32);
        w.u32(10); // GETFH
    });
    let reply = server.rpc_reply(&call(3, 5, &ops)).unwrap();
    let body = reply.body.unwrap();
    assert!(body.len() > COUNT);
    assert!(body.capacity() + reply.header.len() <= LIMIT);
    let replay = server.rpc_reply(&call(4, 5, &ops)).unwrap().body.unwrap();
    assert!(Arc::ptr_eq(&body, &replay));
}

pub(super) fn sequence(w: &mut Writer, session: &[u8; 16], sequence: u32, slot: u32, highest: u32) {
    w.u32(53);
    w.fixed(session);
    for n in [sequence, slot, highest, 1] {
        w.u32(n);
    }
}

fn stat(r: &mut Reader<'_>, opcode: u32, status: u32) {
    assert_eq!((r.u32().unwrap(), r.u32().unwrap()), (opcode, status));
}

fn open_tracked(w: &mut Writer, owner: u32, access: u32, deny: u32, truncate: bool) {
    w.u32(18); // OPEN
    for value in [0, access, deny] {
        w.u32(value);
    }
    w.u64(0);
    w.opaque(&owner.to_be_bytes());
    w.u32(u32::from(truncate));
    if truncate {
        w.u32(0); // UNCHECKED4
        w.u32(1);
        w.u32(1 << 4); // size
        w.opaque(&0u64.to_be_bytes());
    }
    w.u32(0); // CLAIM_NULL
    w.opaque(b"tracked");
}

fn lock_current(w: &mut Writer, owner: u32, kind: u32, offset: u64, length: u64) {
    w.u32(12); // LOCK
    w.u32(kind);
    w.boolean(false);
    w.u64(offset);
    w.u64(length);
    w.boolean(true); // open-to-lock owner
    w.u32(0);
    StateId::CURRENT.encode(w);
    w.u32(0);
    w.u64(0);
    w.opaque(&owner.to_be_bytes());
}

fn reserve(
    server: &mut Server,
    session: &[u8; 16],
    number: u32,
    open_owner: u32,
    lock_owner: Option<u32>,
) {
    let ops = xdr(|w| {
        sequence(w, session, number, 0, 3);
        w.u32(24); // PUTROOTFH
        open_tracked(w, open_owner, 3, 0, false);
        if let Some(lock_owner) = lock_owner {
            lock_current(w, lock_owner, 2, u64::from(number) * 2, 1);
        }
    });
    let count = 3 + u32::from(lock_owner.is_some());
    let reply = server.rpc(&call(number + 2, count, &ops)).unwrap();
    results(&reply, OK, count);
}

#[test]
fn retaining_open_states_does_not_make_the_next_open_retry_forever() {
    let (_dir, mut server) = fixture(WritePolicy::All);
    let session = session(&mut server);
    for number in 1..=4097 {
        reserve(&mut server, &session, number, number, None);
    }
}

#[test]
fn retaining_lock_owners_does_not_make_the_next_lock_retry_forever() {
    let (_dir, mut server) = fixture(WritePolicy::All);
    let session = session(&mut server);
    for number in 1..=4097 {
        reserve(&mut server, &session, number, 1, Some(number));
    }
}

#[test]
fn one_lock_owner_can_hold_many_disjoint_ranges() {
    let (_dir, mut server) = fixture(WritePolicy::All);
    let session = session(&mut server);
    for number in 1..=1025 {
        reserve(&mut server, &session, number, 1, Some(1));
    }
}

#[test]
fn evicted_directory_view_reports_a_bad_cookie() {
    let (_dir, mut server) = fixture(WritePolicy::None);
    let session = session(&mut server);
    let read = |server: &mut Server, number, cookie, verifier: [u8; 8]| {
        let ops = xdr(|w| {
            sequence(w, &session, number, 0, 3);
            w.u32(24); // PUTROOTFH
            w.u32(26); // READDIR
            w.u64(cookie);
            w.fixed(&verifier);
            w.u32(1024);
            w.u32(1024);
            w.u32(0); // empty attribute bitmap
        });
        server.rpc(&call(number + 2, 3, &ops)).unwrap()
    };
    let response = read(&mut server, 1, 0, [0; 8]);
    let mut r = results(&response, OK, 3);
    stat(&mut r, 53, OK);
    r.take(36).unwrap();
    stat(&mut r, 24, OK);
    stat(&mut r, 26, OK);
    let verifier: [u8; 8] = r.fixed().unwrap();
    assert!(r.boolean().unwrap());
    let cookie = r.u64().unwrap();
    let mut wrong = verifier;
    wrong[0] ^= 1;
    let response = read(&mut server, 2, cookie, wrong);
    results(&response, NOT_SAME, 3);
    server.drop_expired_directories(Instant::now(), Duration::ZERO);
    let response = read(&mut server, 3, cookie, verifier);
    results(&response, BAD_COOKIE, 3);
}

#[test]
fn session_replays_an_error_prefix_after_a_mutation() {
    let (_dir, mut server) = fixture(WritePolicy::All);
    let session = session(&mut server);
    let ops = xdr(|w| {
        // The client's target highest slot can be lower during contraction.
        sequence(w, &session, 1, 1, 0);
        w.u32(24);
        w.u32(28);
        w.opaque(b"tracked");
        w.u32(15);
        w.opaque(b"tracked");
        w.u32(10);
    });
    let reply = server.rpc(&call(3, 5, &ops)).unwrap();
    let mut r = results(&reply, NOENT, 4);
    stat(&mut r, 53, OK);
    r.take(36).unwrap();
    stat(&mut r, 24, OK);
    stat(&mut r, 28, OK);
    r.take(20).unwrap();
    stat(&mut r, 15, NOENT);
    r.finish().unwrap();
    assert_eq!(server.rpc(&call(4, 5, &ops)).unwrap()[4..], reply[4..]);
    let other = xdr(|w| {
        sequence(w, &session, 1, 1, 0);
        w.u32(24);
    });
    let reply = server.rpc(&call(5, 2, &other)).unwrap();
    stat(
        &mut results(&reply, SEQ_FALSE_RETRY, 1),
        53,
        SEQ_FALSE_RETRY,
    );
}

#[test]
fn open_upgrades_a_read_reservation_and_truncates() {
    let (_dir, mut server) = fixture(WritePolicy::All);
    let session = session(&mut server);
    for (number, access, truncate) in [(1, 1, false), (2, 2, true)] {
        let ops = xdr(|w| {
            sequence(w, &session, number, 0, 3);
            w.u32(24); // PUTROOTFH
            open_tracked(w, 1, access, 0, truncate);
            w.u32(9); // GETATTR
            w.u32(1);
            w.u32(1 << 4);
        });
        let reply = server.rpc(&call(2 + number, 4, &ops)).unwrap();
        let mut r = results(&reply, OK, 4);
        stat(&mut r, 53, OK);
        r.take(36).unwrap();
        stat(&mut r, 24, OK);
        stat(&mut r, 18, OK);
        assert_eq!(r.u32().unwrap(), number);
        r.take(36).unwrap(); // Stateid suffix, change info, result flags.
        let bitmap_words = r.u32().unwrap();
        r.take(4 * bitmap_words as usize).unwrap();
        assert_eq!(r.u32().unwrap(), 0); // OPEN_DELEGATE_NONE
        stat(&mut r, 9, OK);
        assert_eq!((r.u32().unwrap(), r.u32().unwrap()), (1, 1 << 4));
        let size = u64::from_be_bytes(r.opaque(8).unwrap().try_into().unwrap());
        assert_eq!(
            size,
            if truncate {
                0
            } else {
                b"tracked bytes".len() as u64
            }
        );
        r.finish().unwrap();
    }
}

#[test]
fn same_owner_share_deny_blocks_truncating_open() {
    let (_dir, mut server) = fixture(WritePolicy::All);
    let session = session(&mut server);
    for (number, access, deny, truncate, status) in
        [(1, 1, 2, false, OK), (2, 2, 0, true, SHARE_DENIED)]
    {
        let ops = xdr(|w| {
            sequence(w, &session, number, 0, 3);
            w.u32(24);
            open_tracked(w, 1, access, deny, truncate);
        });
        results(&server.rpc(&call(number + 2, 3, &ops)).unwrap(), status, 3);
    }
    let ino = server
        .core
        .lookup_inode(1, std::ffi::OsStr::new("tracked"))
        .unwrap();
    assert_eq!(
        server.core.nfs_attributes(ino, true).unwrap().0.size,
        b"tracked bytes".len() as u64
    );
}

fn lookup_tracked(w: &mut Writer) {
    w.u32(24); // PUTROOTFH
    w.u32(15); // LOOKUP
    w.opaque(b"tracked");
}

fn write_byte(w: &mut Writer, state: StateId, offset: u64) {
    w.u32(38); // WRITE
    state.encode(w);
    w.u64(offset);
    w.u32(0); // UNSTABLE4
    w.opaque(b"X");
}

#[test]
fn writes_using_read_bypass_obey_anonymous_share_reservations() {
    let (_dir, mut server) = fixture(WritePolicy::All);
    let session = session(&mut server);
    for (number, expected) in [(1, OK), (3, SHARE_DENIED)] {
        if number == 3 {
            let ops = xdr(|w| {
                sequence(w, &session, 2, 0, 3);
                w.u32(24);
                open_tracked(w, 1, 1, 2, false);
            });
            results(&server.rpc(&call(4, 3, &ops)).unwrap(), OK, 3);
        }
        let ops = xdr(|w| {
            sequence(w, &session, number, 0, 3);
            lookup_tracked(w);
            write_byte(w, StateId::READ_BYPASS, 0);
            if number == 1 {
                w.u32(34); // SETATTR
                StateId::READ_BYPASS.encode(w);
                w.u32(2); // FATTR4_MODE (33)
                w.u32(0);
                w.u32(1 << 1);
                w.opaque(&0o640u32.to_be_bytes());
            }
        });
        let count = 4 + u32::from(number == 1);
        results(
            &server.rpc(&call(number + 2, count, &ops)).unwrap(),
            expected,
            count,
        );
    }
}

#[test]
fn a_lock_owner_uses_distinct_states_for_distinct_opens() {
    let (_dir, mut server) = fixture(WritePolicy::All);
    let session = session(&mut server);
    let mut states = Vec::new();
    for number in [1, 2] {
        let ops = xdr(|w| {
            sequence(w, &session, number, 0, 3);
            w.u32(24);
            open_tracked(w, number, number, 0, false);
            lock_current(w, 10, number, u64::from(number) - 1, 2);
        });
        let response = server.rpc(&call(number + 2, 4, &ops)).unwrap();
        let mut r = results(&response, OK, 4);
        stat(&mut r, 53, OK);
        r.take(36).unwrap();
        stat(&mut r, 24, OK);
        stat(&mut r, 18, OK);
        let open = StateId {
            sequence: r.u32().unwrap(),
            other: r.fixed().unwrap(),
        };
        r.take(24).unwrap(); // change information and flags
        let words = r.u32().unwrap();
        r.take(4 * words as usize).unwrap();
        assert_eq!(r.u32().unwrap(), 0); // OPEN_DELEGATE_NONE
        stat(&mut r, 12, OK);
        let lock = StateId {
            sequence: r.u32().unwrap(),
            other: r.fixed().unwrap(),
        };
        r.finish().unwrap();
        states.push((open, lock));
    }
    let ops = xdr(|w| {
        sequence(w, &session, 3, 0, 3);
        lookup_tracked(w);
        write_byte(w, states[1].1, 1);
        w.u32(14); // LOCKU through the first open also changes the common owner.
        w.u32(1);
        w.u32(0);
        states[0].1.encode(w);
        w.u64(2);
        w.u64(1);
    });
    results(&server.rpc(&call(5, 5, &ops)).unwrap(), OK, 5);
    assert_ne!(states[0].1.other, states[1].1.other);

    let ops = xdr(|w| {
        sequence(w, &session, 4, 0, 3);
        lookup_tracked(w);
        w.u32(4); // CLOSE the first open.
        w.u32(0);
        states[0].0.encode(w);
        for offset in [2, 1] {
            w.u32(13); // LOCKT by another owner.
            w.u32(2);
            w.u64(offset);
            w.u64(1);
            w.u64(0);
            w.opaque(b"another");
        }
    });
    results(&server.rpc(&call(6, 6, &ops)).unwrap(), DENIED, 6);
}

#[test]
fn filehandles_are_checked_before_they_can_select_an_inode() {
    let (_dir, mut server) = fixture(WritePolicy::None);
    let session = session(&mut server);
    let mut foreign = server.filehandle(1);
    foreign[0] ^= 1;
    for (i, (bytes, error)) in [
        (b"short".as_slice(), BADHANDLE),
        (&foreign, STALE),
        (&server.filehandle(u64::MAX), STALE),
    ]
    .into_iter()
    .enumerate()
    {
        let ops = xdr(|w| {
            sequence(w, &session, i as u32 + 1, 0, 3);
            w.u32(22);
            w.opaque(bytes);
            w.u32(10);
        });
        let reply = server.rpc(&call(6, 3, &ops)).unwrap();
        let mut r = results(&reply, error, 2);
        stat(&mut r, 53, OK);
        r.take(36).unwrap();
        stat(&mut r, 22, error);
        r.finish().unwrap();
    }
    server.sweep(Instant::now() + Duration::from_secs(91));
    let expired = xdr(|w| sequence(w, &session, 4, 0, 3));
    let reply = server.rpc(&call(7, 1, &expired)).unwrap();
    stat(&mut results(&reply, BADSESSION, 1), 53, BADSESSION);
}

#[test]
fn auth_sys_supplementary_group_grants_group_read() {
    use std::ffi::OsStr;
    let (_dir, mut server) = fixture(WritePolicy::All);
    server
        .core
        .create_node(
            1,
            OsStr::new("group-only"),
            crate::backend::FileType::RegularFile,
            0o040,
            10,
            77,
            Vec::new(),
        )
        .unwrap();
    let session = session_with(&mut server, |xid, count, ops| {
        call_as(xid, count, ops, 20, 21, &[])
    });
    for (number, groups, allowed) in [(1, [].as_slice(), 0), (2, [77].as_slice(), 1)] {
        let ops = xdr(|w| {
            sequence(w, &session, number, 0, 3);
            w.u32(24); // PUTROOTFH
            w.u32(15); // LOOKUP
            w.opaque(b"group-only");
            w.u32(3); // ACCESS
            w.u32(1); // READ
        });
        let reply = server
            .rpc(&call_as(number + 2, 4, &ops, 20, 21, groups))
            .unwrap();
        let mut r = results(&reply, OK, 4);
        stat(&mut r, 53, OK);
        r.take(36).unwrap();
        stat(&mut r, 24, OK);
        stat(&mut r, 15, OK);
        stat(&mut r, 3, OK);
        assert_eq!((r.u32().unwrap(), r.u32().unwrap()), (1, allowed));
        r.finish().unwrap();
    }
}

#[test]
fn direct_removal_cannot_bypass_any_protected_policy() {
    for policy in [
        WritePolicy::None,
        WritePolicy::Ignored,
        WritePolicy::Untracked,
    ] {
        let (_dir, mut server) = fixture(policy);
        let session = session(&mut server);
        let ops = xdr(|w| {
            sequence(w, &session, 1, 0, 3);
            w.u32(24);
            w.u32(28);
            w.opaque(b"tracked");
        });
        let reply = server.rpc(&call(1, 3, &ops)).unwrap();
        results(&reply, ROFS, 3);
        assert!(server.core.lookup_inode(1, "tracked".as_ref()).is_ok());
    }
}

#[test]
fn request_counts_and_lengths_are_bounded() {
    assert!(fragment((MAX_RECORD as u32).to_be_bytes(), 1).is_err());
    assert_eq!(
        fragment(0x8000_0001u32.to_be_bytes(), MAX_RECORD - 1).unwrap(),
        (1, true)
    );
    assert!(Reader::new(&u32::MAX.to_be_bytes())
        .opaque(MAX_RECORD)
        .is_err());
    let (_dir, mut server) = fixture(WritePolicy::None);
    let session = session(&mut server);
    let ops = xdr(|w| sequence(w, &session, 1, 0, 3));
    let reply = server.rpc(&call(1, (MAX_OPS + 1) as u32, &ops)).unwrap();
    stat(&mut results(&reply, TOO_MANY_OPS, 1), 53, TOO_MANY_OPS);
}

#[tokio::test]
async fn unfinished_record_can_be_cancelled() {
    let (mut peer, mut connection) = tokio::io::duplex(8);
    peer.write_all(&0x8000_0010u32.to_be_bytes()).await.unwrap();
    let read = tokio::spawn(async move { read_record(&mut connection).await });
    tokio::task::yield_now().await;
    assert!(!read.is_finished());
    read.abort();
    assert!(read.await.unwrap_err().is_cancelled());
}

#[cfg(feature = "test-metrics")]
#[test]
#[ignore = "run with GIT_ON_TEST_FSYNC_ERROR=1"]
fn sync_errors_are_returned_in_rpc_replies() {
    assert_eq!(std::env::var("GIT_ON_TEST_FSYNC_ERROR").as_deref(), Ok("1"));
    let (_dir, mut server) = fixture(WritePolicy::All);
    let session = session(&mut server);
    for (number, opcode) in [(1, 38), (2, 5)] {
        let ops = xdr(|w| {
            sequence(w, &session, number, 0, 3);
            w.u32(24);
            w.u32(15);
            w.opaque(b"tracked");
            w.u32(opcode);
            if opcode == 38 {
                super::protocol::StateId::ANONYMOUS.encode(w);
                w.u64(0);
                w.u32(2); // FILE_SYNC
                w.opaque(b"written");
            } else {
                w.u64(0);
                w.u32(0);
            }
        });
        let reply = server.rpc(&call(number, 4, &ops)).unwrap();
        let mut r = results(&reply, IO, 4);
        stat(&mut r, 53, OK);
        r.take(36).unwrap();
        stat(&mut r, 24, OK);
        stat(&mut r, 15, OK);
        stat(&mut r, opcode, IO);
        r.finish().unwrap(); // No stable-write or COMMIT verifier on error.
    }
}
