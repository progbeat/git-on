//! In-process NFSv4.1 over the Darwin client's local socket transport.
mod attributes;
mod files;
mod locks;
mod protocol;
mod server;
mod status;
mod xdr;

use self::server::Server;
use crate::{filesystem::Overlay, platform, policy::WritePolicy};
use std::{
    fs::{self, DirBuilder},
    io,
    os::unix::{fs::DirBuilderExt, net::UnixListener as StdListener},
    path::Path,
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicI32, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot},
    task::JoinSet,
};
use xdr::{fragment, MAX_RECORD};

const MAX_CONNECTIONS: usize = 16;
const QUEUE_DEPTH: usize = 8;
const MAX_FRAGMENTS: usize = 256;

#[derive(Default)]
pub(super) struct FilesystemFailures {
    io: AtomicBool,
    writeback: AtomicI32,
}

pub struct Session {
    shutdown: Option<oneshot::Sender<()>>,
    server: Option<JoinHandle<io::Result<()>>>,
    failed: Arc<AtomicBool>,
    failures: Arc<FilesystemFailures>,
}

impl Session {
    pub(super) fn start(overlay: Overlay, id: &str) -> io::Result<Self> {
        let run = uuid::Uuid::parse_str(id).map_err(io::Error::other)?;
        let socket = platform::nfs_socket(id).ok_or(io::ErrorKind::InvalidInput)?;
        let directory = socket.parent().ok_or(io::ErrorKind::InvalidInput)?;
        DirBuilder::new().mode(0o700).create(directory)?;
        let setup = (|| {
            let listener = StdListener::bind(&socket)?;
            listener.set_nonblocking(true)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .max_blocking_threads(1)
                .build()?;
            let (shutdown, stop) = oneshot::channel();
            let failed = Arc::new(AtomicBool::new(false));
            let failures = Arc::new(FilesystemFailures::default());
            let thread_failed = Arc::clone(&failed);
            let filesystem_error = Arc::clone(&failures);
            let server = thread::Builder::new()
                .name("git-on-nfs".into())
                .spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        runtime.block_on(async {
                            let listener = UnixListener::from_std(listener)?;
                            serve(
                                listener,
                                Server::new(overlay, run, filesystem_error),
                                stop,
                                &thread_failed,
                            )
                            .await
                        })
                    }))
                    .unwrap_or_else(|_| Err(io::Error::other("NFS server panicked")));
                    if result.is_err() {
                        notify_failure(&thread_failed);
                    }
                    result
                })?;
            Ok(Self {
                shutdown: Some(shutdown),
                server: Some(server),
                failed,
                failures,
            })
        })();
        if setup.is_err() {
            let _ = fs::remove_file(&socket);
            let _ = fs::remove_dir(directory);
        }
        setup
    }

    pub fn mount_command(path: &Path, id: &str, policy: WritePolicy) -> io::Result<Command> {
        let source = platform::nfs_mount_source(id).ok_or(io::ErrorKind::InvalidInput)?;
        let mut options = String::from(
            "vers=4.1,proto=ticotsord,sec=sys,nocallback,nolocallocks,noquota,retrycnt=1,hard,intr,nodev,nosuid,nobrowse,nonfc,nonamedattr,noacl,nordirplus",
        );
        if policy == WritePolicy::None {
            options.push_str(",ro");
        } else {
            options.push_str(",noac,nonegnamecache");
        }
        let mut cmd = Command::new("/sbin/mount_nfs");
        cmd.args(["-o", &options, &source]).arg(path);
        Ok(cmd)
    }

    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    pub fn filesystem_failed(&self) -> bool {
        self.failures.io.load(Ordering::Relaxed)
            || self.failures.writeback.load(Ordering::Relaxed) != 0
    }

    pub(super) fn join(mut self) -> io::Result<()> {
        self.stop()?;
        let error = self.failures.writeback.load(Ordering::Relaxed);
        if error != 0 {
            let error = io::Error::from_raw_os_error(error);
            return Err(io::Error::new(
                error.kind(),
                format!("NFS writeback failed: {error}"),
            ));
        }
        Ok(())
    }

    fn stop(&mut self) -> io::Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.server.take().map_or(Ok(()), |thread| {
            thread
                .join()
                .map_err(|_| io::Error::other("NFS server panicked"))?
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn notify_failure(failed: &AtomicBool) {
    if !failed.swap(true, Ordering::AcqRel) {
        // The runner already wakes on child-exit notifications.
        unsafe {
            libc::kill(libc::getpid(), libc::SIGCHLD);
        }
    }
}

enum Job {
    Call(Vec<u8>, oneshot::Sender<Option<Vec<u8>>>),
    Sweep,
}

async fn serve(
    listener: UnixListener,
    mut server: Server,
    mut stop: oneshot::Receiver<()>,
    failed: &AtomicBool,
) -> io::Result<()> {
    let (queue, mut jobs) = mpsc::channel(QUEUE_DEPTH);
    let stopped = Arc::new(AtomicBool::new(false));
    let worker_stopped = Arc::clone(&stopped);
    // Overlay already serializes tree and backing mutations. A single bounded
    // queue also keeps native Git waits off the runtime's socket tasks.
    let mut worker = tokio::task::spawn_blocking(move || {
        while let Some(job) = jobs.blocking_recv() {
            if worker_stopped.load(Ordering::Relaxed) {
                break;
            }
            match job {
                Job::Call(data, reply) => {
                    // Once admitted, finish the compound even after disconnect
                    // so a retransmit can use the session slot's cached result.
                    let _ = reply.send(server.rpc(&data));
                }
                Job::Sweep => server.sweep(Instant::now()),
            }
        }
    });
    let mut peers = JoinSet::new();
    let uid = unsafe { libc::geteuid() };
    let mut sweep = tokio::time::interval(Duration::from_secs(5));
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut worker_done = false;
    let result = loop {
        tokio::select! {
            _ = &mut stop => break Ok(()),
            exit = &mut worker => {
                worker_done = true;
                break Err(exit.err().map_or_else(
                    || io::Error::other("NFS worker stopped unexpectedly"),
                    io::Error::other,
                ));
            }
            _ = sweep.tick() => {
                let _ = queue.try_send(Job::Sweep);
            }
            _ = peers.join_next(), if !peers.is_empty() => {},
            accepted = listener.accept(), if peers.len() < MAX_CONNECTIONS => {
                match accepted {
                    Ok((socket, _)) => {
                        if socket.peer_cred().is_ok_and(|peer| peer.uid() == 0 || peer.uid() == uid) {
                            peers.spawn(connection(socket, queue.clone()));
                        }
                    }
                    Err(error) if matches!(error.kind(), io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted) => (),
                    Err(error) => break Err(error),
                }
            }
        }
    };
    if result.is_err() {
        notify_failure(failed);
    }
    drop(listener);
    stopped.store(true, Ordering::Relaxed);
    peers.shutdown().await;
    drop(queue);
    if !worker_done {
        worker.await.map_err(io::Error::other)?;
    }
    result
}

async fn connection(mut socket: UnixStream, queue: mpsc::Sender<Job>) -> io::Result<()> {
    loop {
        // One record per permitted connection provides backpressure before
        // queue admission. A stalled peer may keep a connection for two leases.
        let idle = Duration::from_secs(u64::from(server::LEASE_SECONDS) * 2);
        let data = tokio::time::timeout(idle, read_record(&mut socket)).await??;
        let (send, receive) = oneshot::channel();
        queue
            .send(Job::Call(data, send))
            .await
            .map_err(io::Error::other)?;
        if let Some(response) = receive.await.map_err(io::Error::other)? {
            if response.len() > MAX_RECORD {
                return Err(io::Error::other("NFS response exceeds record limit"));
            }
            tokio::time::timeout(idle, async {
                socket
                    .write_all(&(0x8000_0000 | response.len() as u32).to_be_bytes())
                    .await?;
                socket.write_all(&response).await
            })
            .await??;
        }
    }
}

async fn read_record(socket: &mut (impl tokio::io::AsyncRead + Unpin)) -> io::Result<Vec<u8>> {
    let mut record = Vec::new();
    for _ in 0..MAX_FRAGMENTS {
        let mut header = [0; 4];
        socket.read_exact(&mut header).await?;
        let start = record.len();
        let (len, last) = fragment(header, start).map_err(|_| io::ErrorKind::InvalidData)?;
        record.resize(start + len, 0);
        socket.read_exact(&mut record[start..]).await?;
        if last {
            return Ok(record);
        }
    }
    Err(io::ErrorKind::InvalidData.into())
}

#[cfg(test)]
mod tests;
