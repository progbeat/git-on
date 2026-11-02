#![cfg(all(feature = "async", target_os = "linux"))]
use fuser::{
    AsyncFilesystem, AsyncSession, Config, Errno, FileHandle, INodeNo, LockOwner, OpenFlags,
    Request, SessionACL,
    reply_async::{AttrResponse, ReadResponse},
};
use std::{cell::Cell, os::fd::OwnedFd, rc::Rc, time::Duration};
use tokio::{net::UnixDatagram, sync::Notify};

struct DropRead(Rc<Cell<bool>>);
impl Drop for DropRead {
    fn drop(&mut self) {
        self.0.set(false);
    }
}
struct Fs {
    waiting: Rc<Cell<bool>>,
    destroyed: Rc<Cell<bool>>,
    resume: Rc<Notify>,
    shutdown: Rc<Cell<bool>>,
}
impl AsyncFilesystem for Fs {
    async fn read(
        &self,
        _: &Request,
        _: INodeNo,
        _: FileHandle,
        _: u64,
        _: u32,
        _: OpenFlags,
        _: Option<LockOwner>,
    ) -> Result<ReadResponse, Errno> {
        self.waiting.set(true);
        let _read = DropRead(self.waiting.clone());
        self.resume.notified().await;
        Ok(ReadResponse::new(b"verified bytes".to_vec()))
    }
    async fn getattr(
        &self,
        _: &Request,
        _: INodeNo,
        _: Option<FileHandle>,
    ) -> Result<AttrResponse, Errno> {
        assert!(
            self.waiting.get(),
            "lookup must execute while read is suspended"
        );
        Err(Errno::ENOENT)
    }
    async fn shutdown(&mut self) -> std::io::Result<()> {
        assert!(!self.waiting.get(), "shutdown ran before requests drained");
        tokio::task::yield_now().await;
        self.shutdown.set(true);
        Ok(())
    }
    fn destroy(&mut self) {
        assert!(
            self.shutdown.get(),
            "destroy ran before asynchronous shutdown completed"
        );
        assert!(
            !self.waiting.get(),
            "destroy ran before the read future ended"
        );
        self.destroyed.set(true);
    }
}
fn packet(op: u32, unique: u64, body: &[u8]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend(((40 + body.len()) as u32).to_ne_bytes());
    p.extend(op.to_ne_bytes());
    p.extend(unique.to_ne_bytes());
    p.extend(2u64.to_ne_bytes());
    for x in [
        unsafe { libc::geteuid() },
        unsafe { libc::getegid() },
        std::process::id(),
        0,
    ] {
        p.extend(x.to_ne_bytes())
    }
    p.extend(body);
    p
}
async fn receive(socket: &UnixDatagram) -> (u64, i32, Vec<u8>) {
    let mut b = vec![0; 4096];
    let n = socket.recv(&mut b).await.unwrap();
    assert!(n >= 16);
    b.truncate(n);
    (
        u64::from_ne_bytes(b[8..16].try_into().unwrap()),
        i32::from_ne_bytes(b[4..8].try_into().unwrap()),
        b[16..].to_vec(),
    )
}
#[tokio::test(flavor = "current_thread")]
async fn pending_read_allows_other_requests_and_finishes_before_destroy() {
    let (client, server) = std::os::unix::net::UnixDatagram::pair().unwrap();
    client.set_nonblocking(true).unwrap();
    let client = UnixDatagram::from_std(client).unwrap();
    let fd: OwnedFd = server.into();
    let waiting = Rc::new(Cell::new(false));
    let destroyed = Rc::new(Cell::new(false));
    let resume = Rc::new(Notify::new());
    let fs = Fs {
        waiting: waiting.clone(),
        destroyed: destroyed.clone(),
        resume: resume.clone(),
        shutdown: Rc::new(Cell::new(false)),
    };
    let server = async {
        AsyncSession::from_fd(fs, fd, SessionACL::All, Config::default())
            .await
            .unwrap()
            .run()
            .await
            .unwrap();
    };
    let client = async {
        let init = [7u32, 31, 0, 1];
        let init = init
            .into_iter()
            .flat_map(u32::to_ne_bytes)
            .collect::<Vec<_>>();
        client.send(&packet(26, 1, &init)).await.unwrap();
        assert_eq!(receive(&client).await.0, 1);
        let mut read = Vec::new();
        read.extend(0u64.to_ne_bytes());
        read.extend(0u64.to_ne_bytes());
        read.extend(64u32.to_ne_bytes());
        read.extend(0u32.to_ne_bytes());
        read.extend(0u64.to_ne_bytes());
        read.extend(0u32.to_ne_bytes());
        read.extend(0u32.to_ne_bytes());
        client.send(&packet(15, 2, &read)).await.unwrap();
        while !waiting.get() {
            tokio::task::yield_now().await
        }
        client.send(&packet(3, 3, &[0; 16])).await.unwrap();
        let (id, error, _) = receive(&client).await;
        assert_eq!(id, 3);
        assert_eq!(error, -libc::ENOENT);
        resume.notify_one();
        let (id, error, bytes) = receive(&client).await;
        assert_eq!(id, 2);
        assert_eq!(error, 0);
        assert_eq!(bytes, b"verified bytes");
        client.send(&packet(38, 4, &[])).await.unwrap();
        assert_eq!(receive(&client).await.0, 4);
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(server, client)
    })
    .await
    .expect("a pending read blocked independent FUSE requests");
    assert!(destroyed.get());
    assert!(!waiting.get());
}
