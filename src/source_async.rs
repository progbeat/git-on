//! Async boundary for libgit2's blocking decoder. One owned source thread handles
//! cold demands; namespace, headers and the payload LRU stay on the local loop.
//! No core/cache mutex or per-request blocking pool crosses this boundary.
use crate::{raw_blob::BLOCK, tree::Directory};
use anyhow::{Context, Result};
use git2::{ObjectType, Oid, Repository};
use std::{
    io::{self, Write},
    thread::JoinHandle,
};
use tokio::sync::{mpsc, oneshot};

/// Only the source decoder can produce this proof, after the complete canonical
/// object has passed verification. Consumers must not publish preceding chunks
/// until this final event arrives successfully.
pub(crate) struct Verified {
    oid: Oid,
    size: usize,
}
pub(crate) struct SmallBlob {
    pub(crate) bytes: Vec<u8>,
    pub(crate) proof: Verified,
}
impl Verified {
    pub(crate) fn oid(&self) -> Oid {
        self.oid
    }
    pub(crate) fn size(&self) -> usize {
        self.size
    }
}
pub(crate) enum BlobEvent {
    Chunk(Vec<u8>),
    Verified(Verified),
    Failed(anyhow::Error),
}
pub(crate) struct BlobStream {
    pub(crate) size: usize,
    events: mpsc::Receiver<BlobEvent>,
}
impl BlobStream {
    pub(crate) async fn next(&mut self) -> Option<BlobEvent> {
        self.events.recv().await
    }
}
type Headers = Vec<(Oid, Result<(usize, ObjectType)>)>;
enum Demand {
    Header(Oid, oneshot::Sender<Result<(usize, ObjectType)>>),
    Headers(Vec<Oid>, oneshot::Sender<Result<Headers>>),
    Tree(Oid, oneshot::Sender<Result<Directory>>),
    Blob(Oid, Option<usize>, oneshot::Sender<Result<BlobStream>>),
    SmallBlob(Oid, usize, oneshot::Sender<Result<SmallBlob>>),
    #[cfg(test)]
    Pause(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>),
}
fn copy_blob(
    repository: &Repository,
    oid: Oid,
    size: usize,
    writer: &mut impl Write,
) -> Result<()> {
    let odb = repository.odb()?;
    let result = if let Ok((reader, length, kind)) = odb.reader(oid) {
        anyhow::ensure!(
            kind == ObjectType::Blob && length == size,
            "Git blob changed during transfer"
        );
        crate::object::copy_verified(reader, writer, ObjectType::Blob, size, oid)
    } else {
        let object = odb.read(oid)?;
        anyhow::ensure!(
            object.kind() == ObjectType::Blob && object.len() == size,
            "Git blob changed during transfer"
        );
        crate::object::copy_odb_blob(&object, writer)
    };
    result
}
pub(crate) fn read_tree(repository: &Repository, oid: Oid) -> Result<Directory> {
    let odb = repository.odb()?;
    let (size, kind) = odb.read_header(oid)?;
    anyhow::ensure!(
        kind == ObjectType::Tree,
        "snapshot directory does not reference a tree"
    );
    let parsed = if let Ok((reader, length, kind)) = odb.reader(oid) {
        anyhow::ensure!(
            kind == ObjectType::Tree && length == size,
            "Git tree changed during transfer"
        );
        Directory::verified(oid, size, reader)
    } else {
        let object = odb.read(oid)?;
        anyhow::ensure!(
            object.kind() == ObjectType::Tree && object.len() == size,
            "Git tree changed during transfer"
        );
        Directory::verified(oid, size, object.data())
    };
    parsed
}
struct ChunkWriter {
    sender: mpsc::Sender<BlobEvent>,
    bytes: Vec<u8>,
}
impl ChunkWriter {
    fn emit(&mut self) -> io::Result<()> {
        if self.bytes.is_empty() {
            return Ok(());
        }
        let bytes = std::mem::take(&mut self.bytes);
        self.sender
            .blocking_send(BlobEvent::Chunk(bytes))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "object demand was cancelled"))
    }
}
impl Write for ChunkWriter {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let length = bytes.len();
        while !bytes.is_empty() {
            if self.bytes.capacity() == 0 {
                self.bytes.reserve_exact(bytes.len().min(BLOCK));
            }
            let count = bytes.len().min(BLOCK - self.bytes.len());
            self.bytes.extend_from_slice(&bytes[..count]);
            bytes = &bytes[count..];
            if self.bytes.len() == BLOCK {
                self.emit()?;
            }
        }
        Ok(length)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub(crate) struct Source {
    sender: Option<mpsc::Sender<Demand>>,
    done: Option<oneshot::Receiver<Repository>>,
    thread: Option<JoinHandle<()>>,
    #[cfg(test)]
    demands: std::cell::Cell<usize>,
}
impl Source {
    pub(crate) fn new(repository: Repository) -> io::Result<Self> {
        let (sender, mut receiver) = mpsc::channel(16);
        let (done_tx, done) = oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("git-on-source".into())
            .spawn(move || {
                while let Some(demand) = receiver.blocking_recv() {
                    match demand {
                        Demand::Headers(oids, reply) => {
                            let result = repository.odb().map_err(Into::into).map(|odb| {
                                oids.into_iter()
                                    .map(|oid| (oid, odb.read_header(oid).map_err(Into::into)))
                                    .collect()
                            });
                            let _ = reply.send(result);
                        }
                        Demand::Header(oid, reply) => {
                            let _ = reply.send(
                                repository
                                    .odb()
                                    .map_err(Into::into)
                                    .and_then(|odb| odb.read_header(oid).map_err(Into::into)),
                            );
                        }
                        Demand::SmallBlob(oid, size, reply) => {
                            let result = (|| {
                                crate::object::check_transfer()?;
                                anyhow::ensure!(
                                    size <= BLOCK,
                                    "small object demand exceeds stream boundary"
                                );
                                let mut bytes = Vec::with_capacity(size);
                                // This reply already needs the complete bounded object.
                                // Packed backends do not provide ODB streams.
                                let odb = repository.odb()?;
                                let object = odb.read(oid)?;
                                anyhow::ensure!(
                                    object.kind() == ObjectType::Blob && object.len() == size,
                                    "Git blob changed during transfer"
                                );
                                crate::object::copy_odb_blob(&object, &mut bytes)?;
                                Ok(SmallBlob {
                                    bytes,
                                    proof: Verified { oid, size },
                                })
                            })();
                            let _ = reply.send(result);
                        }
                        Demand::Tree(oid, reply) => {
                            let result = read_tree(&repository, oid);
                            let _ = reply.send(result);
                        }
                        Demand::Blob(oid, known_size, reply) => {
                            let result = (|| -> Result<_> {
                                crate::object::check_transfer()?;
                                if let Some(size) = known_size {
                                    return Ok(size);
                                }
                                let odb = repository.odb()?;
                                let (size, kind) = odb.read_header(oid)?;
                                anyhow::ensure!(
                                    kind == ObjectType::Blob,
                                    "snapshot file does not reference a blob"
                                );
                                Ok(size)
                            })();
                            let size = match result {
                                Ok(size) => size,
                                Err(error) => {
                                    let _ = reply.send(Err(error));
                                    continue;
                                }
                            };
                            let (tx, events) = mpsc::channel(2);
                            if reply.send(Ok(BlobStream { size, events })).is_err() {
                                continue;
                            }
                            let mut writer = ChunkWriter {
                                sender: tx,
                                bytes: Vec::with_capacity(size.min(BLOCK)),
                            };
                            let transfer = (|| -> Result<()> {
                                copy_blob(&repository, oid, size, &mut writer)?;
                                writer.emit()?;
                                Ok(())
                            })();
                            let event = match transfer {
                                Ok(()) => BlobEvent::Verified(Verified { oid, size }),
                                Err(error) => BlobEvent::Failed(error),
                            };
                            let _ = writer.sender.blocking_send(event);
                        }
                        #[cfg(test)]
                        Demand::Pause(started, resume) => {
                            let _ = started.send(());
                            let _ = resume.recv();
                        }
                    }
                }
                let _ = done_tx.send(repository);
            })?;
        Ok(Self {
            sender: Some(sender),
            done: Some(done),
            thread: Some(thread),
            #[cfg(test)]
            demands: std::cell::Cell::new(0),
        })
    }
    async fn demand<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> Demand,
    ) -> Result<T> {
        #[cfg(test)]
        self.demands.set(self.demands.get() + 1);
        let (tx, rx) = oneshot::channel();
        self.sender
            .as_ref()
            .context("source is closed")?
            .send(make(tx))
            .await
            .map_err(|_| anyhow::anyhow!("source decoder stopped"))?;
        rx.await
            .context("source decoder stopped without completing its demand")?
    }
    #[cfg(test)]
    pub(crate) fn tested_demands(&self) -> usize {
        self.demands.get()
    }
    #[cfg(test)]
    pub(crate) async fn tested_pause(&self) -> std::sync::mpsc::Sender<()> {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        self.sender
            .as_ref()
            .unwrap()
            .send(Demand::Pause(started_tx, resume_rx))
            .await
            .unwrap();
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        resume_tx
    }
    pub(crate) async fn header(&self, oid: Oid) -> Result<(usize, ObjectType)> {
        self.demand(|reply| Demand::Header(oid, reply)).await
    }
    pub(crate) async fn headers(&self, oids: Vec<Oid>) -> Result<Headers> {
        self.demand(|reply| Demand::Headers(oids, reply)).await
    }
    pub(crate) async fn tree(&self, oid: Oid) -> Result<Directory> {
        self.demand(|reply| Demand::Tree(oid, reply)).await
    }
    pub(crate) async fn blob(&self, oid: Oid) -> Result<BlobStream> {
        self.demand(|reply| Demand::Blob(oid, None, reply)).await
    }
    pub(crate) async fn blob_with_size(&self, oid: Oid, size: usize) -> Result<BlobStream> {
        self.demand(|reply| Demand::Blob(oid, Some(size), reply))
            .await
    }
    pub(crate) async fn small_blob(&self, oid: Oid, size: usize) -> Result<SmallBlob> {
        anyhow::ensure!(size <= BLOCK, "small object demand exceeds stream boundary");
        self.demand(|reply| Demand::SmallBlob(oid, size, reply))
            .await
    }
    /// Close intake, finish owned demands and return the decoder's repository.
    /// The loop must drop/drain its BlobStreams before calling this shutdown.
    pub(crate) async fn finish(mut self) -> Result<Repository> {
        self.sender.take();
        let repository = self
            .done
            .take()
            .unwrap()
            .await
            .context("source decoder panicked")?;
        self.thread
            .take()
            .unwrap()
            .join()
            .map_err(|_| anyhow::anyhow!("source decoder panicked"))?;
        Ok(repository)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn small_source_reply_contains_only_completely_verified_bounded_contents() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(dir.path()).unwrap();
        let bytes = vec![b'x'; BLOCK];
        let oid = repo.blob(&bytes).unwrap();
        let source = Source::new(repo).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let blob = source.small_blob(oid, BLOCK).await.unwrap();
                assert_eq!(blob.bytes, bytes);
                assert_eq!((blob.proof.oid(), blob.proof.size()), (oid, BLOCK));
                assert!(
                    source.small_blob(oid, BLOCK - 1).await.is_err(),
                    "a size mismatch published a verified reply"
                );
                let demands = source.tested_demands();
                assert!(source.small_blob(oid, BLOCK + 1).await.is_err());
                assert_eq!(
                    source.tested_demands(),
                    demands,
                    "oversized demand reached the bounded source path"
                );
                source.finish().await.unwrap();
            });
    }

    #[test]
    fn packed_small_reply_checks_contents_kind_and_declared_length() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(dir.path()).unwrap();
        let bytes = (0..BLOCK)
            .map(|i| (i.wrapping_mul(37) >> 5) as u8)
            .collect::<Vec<_>>();
        let oid = repo.blob(&bytes).unwrap();
        let empty = repo.blob(&[]).unwrap();
        let tree = repo.treebuilder(None).unwrap().write().unwrap();
        {
            let mut pack = repo.packbuilder().unwrap();
            for id in [oid, empty, tree] {
                pack.insert_object(id, None).unwrap();
            }
            let mut data = git2::Buf::new();
            pack.write_buf(&mut data).unwrap();
            let odb = repo.odb().unwrap();
            let mut writer = odb.packwriter().unwrap();
            writer.write_all(&data).unwrap();
            writer.commit().unwrap();
        }
        for id in [oid, empty, tree] {
            let text = id.to_string();
            std::fs::remove_file(
                repo.path()
                    .join("objects")
                    .join(&text[..2])
                    .join(&text[2..]),
            )
            .unwrap();
        }
        let source = Source::new(repo).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let blob = source.small_blob(oid, bytes.len()).await.unwrap();
                assert_eq!(blob.bytes, bytes);
                assert_eq!((blob.proof.oid(), blob.proof.size()), (oid, BLOCK));
                assert!(source.small_blob(oid, BLOCK - 1).await.is_err());
                assert!(source.small_blob(tree, 0).await.is_err());
                let blob = source.small_blob(empty, 0).await.unwrap();
                assert!(blob.bytes.is_empty());
                assert_eq!((blob.proof.oid(), blob.proof.size()), (empty, 0));
                source.finish().await.unwrap();
            });
    }

    #[test]
    fn cold_source_wait_does_not_block_the_local_loop() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(dir.path()).unwrap();
        let oid = repo.blob(b"small verified contents").unwrap();
        let source = Source::new(repo).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (started_tx, started_rx) = std::sync::mpsc::channel();
                let (resume_tx, resume_rx) = std::sync::mpsc::channel();
                source
                    .sender
                    .as_ref()
                    .unwrap()
                    .send(Demand::Pause(started_tx, resume_rx))
                    .await
                    .unwrap();
                // Waiting on this test barrier only confirms the native decoder is paused;
                // the independent task below must run while a real header demand awaits it.
                started_rx
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap();
                let request = source.header(oid);
                let release = async {
                    tokio::task::yield_now().await;
                    resume_tx.send(()).unwrap();
                };
                let (result, ()) = tokio::join!(request, release);
                assert_eq!(result.unwrap(), (23, ObjectType::Blob));
                let mut stream = source.blob(oid).await.unwrap();
                let mut bytes = Vec::new();
                let mut verified = false;
                while let Some(event) = stream.next().await {
                    match event {
                        BlobEvent::Chunk(b) => {
                            assert!(!verified);
                            bytes.extend(b)
                        }
                        BlobEvent::Verified(p) => {
                            assert_eq!(p.oid(), oid);
                            assert_eq!(p.size(), bytes.len());
                            verified = true
                        }
                        BlobEvent::Failed(e) => panic!("{e:#}"),
                    }
                }
                assert!(verified);
                assert_eq!(bytes, b"small verified contents");
                drop(stream);
                source.finish().await.unwrap();
            });
    }
    #[test]
    fn coalesced_ram_loads_and_hits_do_not_wait_for_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(dir.path()).unwrap();
        let bytes = b"small verified contents";
        let oid = repo.blob(bytes).unwrap();
        let tree = {
            let mut tree = repo.treebuilder(None).unwrap();
            tree.insert("file", oid, 0o100644).unwrap();
            tree.write().unwrap()
        };
        let source = Source::new(repo).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let cache = crate::raw_blob_async::RawBlobs::new(dir.path()).unwrap();
                let (a, b) = tokio::join!(cache.prepare(&source, oid), cache.prepare(&source, oid));
                a.unwrap();
                b.unwrap();
                assert_eq!(
                    source.tested_demands(),
                    1,
                    "the same cold OID was decoded twice"
                );
                assert_eq!(cache.tested_io_operations(), 0);
                let (started_tx, started_rx) = std::sync::mpsc::channel();
                let (resume_tx, resume_rx) = std::sync::mpsc::channel();
                source
                    .sender
                    .as_ref()
                    .unwrap()
                    .send(Demand::Pause(started_tx, resume_rx))
                    .await
                    .unwrap();
                started_rx
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap();
                let header = source.header(oid);
                let hit = async {
                    cache.prepare(&source, oid).await.unwrap();
                    assert_eq!(cache.read(oid, 0, 100).await.unwrap().as_slice(), bytes);
                    assert_eq!(cache.tested_io_operations(), 0);
                    resume_tx.send(()).unwrap();
                };
                let (header, ()) = tokio::join!(header, hit);
                assert_eq!(header.unwrap(), (bytes.len(), ObjectType::Blob));
                assert_eq!(
                    source.tested_demands(),
                    2,
                    "a RAM hit submitted a source operation"
                );
                let directory = source.tree(tree).await.unwrap();
                assert_eq!(
                    directory.lookup(std::ffi::OsStr::new("file")),
                    Some((0o100644, oid))
                );
                source.finish().await.unwrap();
            });
    }
    #[test]
    fn large_source_stream_is_bounded_and_cancelled_consumers_do_not_stall_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(dir.path()).unwrap();
        let bytes = vec![b'q'; BLOCK * 128 + 17];
        let oid = repo.blob(&bytes).unwrap();
        let source = Source::new(repo).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut stream = source.blob(oid).await.unwrap();
                assert_eq!(stream.size, bytes.len());
                let mut count = 0;
                let mut verified = false;
                while let Some(event) = stream.next().await {
                    match event {
                        BlobEvent::Chunk(b) => {
                            assert!(b.len() <= BLOCK);
                            assert!(b.iter().all(|&v| v == b'q'));
                            count += b.len()
                        }
                        BlobEvent::Verified(p) => {
                            assert_eq!(p.oid(), oid);
                            assert_eq!(p.size(), count);
                            verified = true
                        }
                        BlobEvent::Failed(e) => panic!("{e:#}"),
                    }
                }
                assert!(verified);
                assert_eq!(count, bytes.len());
                drop(stream);
                let cancelled = source.blob(oid).await.unwrap();
                drop(cancelled);
                assert_eq!(source.header(oid).await.unwrap().0, bytes.len());
                source.finish().await.unwrap();
            });
    }
}
