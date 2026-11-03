//! Blocking native transport/decoder boundary. Verified payload ownership moves
//! into the event loop; the source does not retain a second payload LRU.
use crate::{
    raw_blob::{BlobHandoff, PreparedBlob},
    remote::Remote,
    tree::Directory,
};
use anyhow::{Context, Result};
use git2::{ObjectType, Oid, Repository};
use std::{sync::Arc, thread::JoinHandle};
use tokio::sync::{mpsc, oneshot};

const MAX_DEMANDS: usize = 16;
enum Demand {
    Tree(Oid, oneshot::Sender<Result<(Arc<Directory>, Batch)>>),
    Blob(Oid, oneshot::Sender<Result<Batch>>),
    #[cfg(test)]
    Pause(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>),
}
pub(crate) struct Batch {
    pub(crate) size: u64,
    blobs: Vec<PreparedBlob>,
    returns: std::sync::mpsc::Sender<PreparedBlob>,
    resume: Option<std::sync::mpsc::Sender<()>>,
    published: Option<(std::sync::mpsc::Sender<Oid>, Oid)>,
}
impl Batch {
    pub(crate) fn next(&mut self) -> Option<BlobHandoff> {
        self.blobs
            .pop()
            .map(|blob| BlobHandoff::new(blob, self.returns.clone()))
    }
}
impl Drop for Batch {
    fn drop(&mut self) {
        for blob in self.blobs.drain(..) {
            let _ = self.returns.send(blob);
        }
        if let Some((published, oid)) = self.published.take() {
            let _ = published.send(oid);
        }
        if let Some(resume) = self.resume.take() {
            let _ = resume.send(());
        }
    }
}
enum Backpressure {
    None,
    Pending(Oid),
    Wait(std::sync::mpsc::Receiver<()>),
}
impl Backpressure {
    fn retain_or_wait(self, pending: &mut Vec<Oid>) {
        match self {
            Self::None => {}
            Self::Pending(oid) => pending.push(oid),
            Self::Wait(wait) => {
                let _ = wait.recv();
            }
        }
    }
}
pub(crate) struct Source {
    sender: Option<mpsc::Sender<Demand>>,
    done: Option<oneshot::Receiver<Remote>>,
    thread: Option<JoinHandle<()>>,
    #[cfg(test)]
    demands: std::cell::Cell<usize>,
}
fn tree(repo: &Repository, remote: &mut Remote, oid: Oid) -> Result<Arc<Directory>> {
    remote.ensure_in(repo, oid)?;
    if let Some(raw) = remote.raw_blobs.get(oid)? {
        anyhow::ensure!(
            raw.kind() == ObjectType::Tree,
            "snapshot directory does not reference a tree"
        );
    }
    if let Some((tree, _)) = remote.raw_blobs.directory(oid) {
        return Ok(tree);
    }
    crate::source_async::read_tree(repo, oid).map(Arc::new)
}
fn blob(repo: &Repository, remote: &mut Remote, oid: Oid) -> Result<u64> {
    remote.ensure_in(repo, oid)?;
    if remote.raw_blobs.get(oid)?.is_none() {
        let odb = repo.odb()?;
        let (size, kind) = odb.read_header(oid)?;
        anyhow::ensure!(
            kind == ObjectType::Blob,
            "snapshot file does not reference a blob"
        );
        if let Ok((reader, length, kind)) = odb.reader(oid) {
            anyhow::ensure!(
                kind == ObjectType::Blob && length == size,
                "Git blob changed during transfer"
            );
            remote
                .raw_blobs
                .insert_fetched(oid, size, reader, &remote.dir)?;
        } else {
            let object = odb.read(oid)?;
            remote.raw_blobs.insert_odb_blob(oid, &object)?;
        };
    }
    let prepared = remote
        .raw_blobs
        .get(oid)?
        .context("missing prepared remote blob")?;
    anyhow::ensure!(
        prepared.kind() == ObjectType::Blob,
        "snapshot file does not reference a blob"
    );
    Ok(prepared.len())
}
fn batch(
    remote: &mut Remote,
    returns: &std::sync::mpsc::Sender<PreparedBlob>,
    published: &std::sync::mpsc::Sender<Oid>,
    size: u64,
) -> Result<(Batch, Backpressure)> {
    let mut prepared = remote.raw_blobs.take_ready()?;
    let weight: usize = prepared.iter().map(PreparedBlob::resident_weight).sum();
    let (resume, publication, wait) = if prepared.len() == 1
        && weight <= crate::raw_blob::BLOCK + crate::raw_blob::ENTRY_CHARGE
    {
        let oid = prepared[0].oid();
        (
            None,
            Some((published.clone(), oid)),
            Backpressure::Pending(oid),
        )
    } else if !prepared.is_empty() {
        let (resume, wait) = std::sync::mpsc::channel();
        (Some(resume), None, Backpressure::Wait(wait))
    } else {
        (None, None, Backpressure::None)
    };
    for blob in &mut prepared {
        blob.mark_unread();
    }
    Ok((
        Batch {
            size,
            blobs: prepared,
            returns: returns.clone(),
            resume,
            published: publication,
        },
        wait,
    ))
}
impl Source {
    pub(crate) fn new(repo: Repository, mut remote: Remote) -> Result<(Self, Vec<PreparedBlob>)> {
        let initial = remote.raw_blobs.begin_async()?;
        let (sender, mut receiver) = mpsc::channel(MAX_DEMANDS);
        let (done_tx, done) = oneshot::channel();
        let (returns, returned) = std::sync::mpsc::channel();
        let (published, publications) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("git-on-remote".into())
            .spawn(move || {
                let mut pending = Vec::with_capacity(MAX_DEMANDS);
                let restore = |remote: &mut Remote| {
                    for prepared in returned.try_iter() {
                        if let Err(error) = remote.raw_blobs.restore_blob(prepared) {
                            remote.record_async_failure(&error);
                        }
                    }
                };
                while let Some(demand) = receiver.blocking_recv() {
                    let wanted = match &demand {
                        Demand::Tree(oid, _) | Demand::Blob(oid, _) => Some(*oid),
                        #[cfg(test)]
                        Demand::Pause(..) => None,
                    };
                    // Small deliveries do not wake this worker or require an
                    // acknowledgement round trip before unrelated cold work.
                    // An exported header is usable only after its owning loop
                    // has published the payload (or returned a cancelled one).
                    for oid in publications.try_iter() {
                        pending.retain(|pending| *pending != oid);
                    }
                    while wanted.is_some_and(|oid| pending.contains(&oid))
                        || pending.len() == MAX_DEMANDS
                    {
                        let oid = publications.recv().expect("publication channel closed");
                        pending.retain(|pending| *pending != oid);
                    }
                    restore(&mut remote);
                    match demand {
                        Demand::Tree(oid, reply) => {
                            let result = tree(&repo, &mut remote, oid).and_then(|tree| {
                                batch(&mut remote, &returns, &published, 0)
                                    .map(|(batch, wait)| ((tree, batch), wait))
                            });
                            let result = match result {
                                Ok((packet, wait)) => {
                                    let _ = reply.send(Ok(packet));
                                    wait.retain_or_wait(&mut pending);
                                    continue;
                                }
                                Err(error) => Err(error),
                            };
                            if let Err(error) = &result {
                                remote.record_async_failure(error);
                            }
                            let _ = reply.send(result);
                        }
                        Demand::Blob(oid, reply) => {
                            let result = blob(&repo, &mut remote, oid)
                                .and_then(|size| batch(&mut remote, &returns, &published, size));
                            let result = match result {
                                Ok((packet, wait)) => {
                                    let _ = reply.send(Ok(packet));
                                    wait.retain_or_wait(&mut pending);
                                    continue;
                                }
                                Err(error) => Err(error),
                            };
                            if let Err(error) = &result {
                                remote.record_async_failure(error);
                            }
                            let _ = reply.send(result);
                        }
                        #[cfg(test)]
                        Demand::Pause(started, resume) => {
                            let _ = started.send(());
                            let _ = resume.recv();
                        }
                    }
                }
                restore(&mut remote);
                drop(repo);
                if let Err(error) = remote.stop_transport() {
                    remote.record_async_failure(&error);
                }
                let _ = done_tx.send(remote);
            })?;
        Ok((
            Self {
                sender: Some(sender),
                done: Some(done),
                thread: Some(thread),
                #[cfg(test)]
                demands: std::cell::Cell::new(0),
            },
            initial,
        ))
    }
    async fn demand<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> Demand,
    ) -> Result<T> {
        #[cfg(test)]
        self.demands.set(self.demands.get() + 1);
        let (reply, wait) = oneshot::channel();
        self.sender
            .as_ref()
            .context("remote source is closed")?
            .send(make(reply))
            .await
            .map_err(|_| anyhow::anyhow!("remote source stopped"))?;
        wait.await
            .context("remote source stopped before completing its demand")?
    }
    pub(crate) async fn tree(&self, oid: Oid) -> Result<(Arc<Directory>, Batch)> {
        self.demand(|reply| Demand::Tree(oid, reply)).await
    }
    pub(crate) async fn blob(&self, oid: Oid) -> Result<Batch> {
        self.demand(|reply| Demand::Blob(oid, reply)).await
    }
    #[cfg(test)]
    pub(crate) fn tested_demands(&self) -> usize {
        self.demands.get()
    }
    #[cfg(test)]
    pub(crate) async fn tested_pause(&self) -> std::sync::mpsc::Sender<()> {
        let (started, ready) = std::sync::mpsc::channel();
        let (resume, wait) = std::sync::mpsc::channel();
        self.sender
            .as_ref()
            .unwrap()
            .send(Demand::Pause(started, wait))
            .await
            .unwrap();
        ready
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        resume
    }
    pub(crate) async fn finish(mut self) -> Result<Remote> {
        self.sender.take();
        let remote = self
            .done
            .take()
            .unwrap()
            .await
            .context("remote source panicked")?;
        self.thread
            .take()
            .unwrap()
            .join()
            .map_err(|_| anyhow::anyhow!("remote source panicked"))?;
        Ok(remote)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsStr, rc::Rc, time::Duration};

    #[test]
    fn small_delivery_must_be_published_before_a_duplicate_demand_completes() {
        let temp = tempfile::tempdir().unwrap();
        let repository = Repository::init_bare(temp.path().join("server.git")).unwrap();
        let mut config = repository.config().unwrap();
        config.set_bool("uploadpack.allowFilter", true).unwrap();
        config
            .set_bool("uploadpack.allowAnySHA1InWant", true)
            .unwrap();
        let bytes = b"one small verified object";
        let oid = repository.blob(bytes).unwrap();
        let other = repository.blob(b"independent cold object").unwrap();
        let mut tree = repository.treebuilder(None).unwrap();
        tree.insert("file", oid, 0o100644).unwrap();
        tree.insert("other", other, 0o100644).unwrap();
        let root = tree.write().unwrap();
        let url = format!("file://{}", repository.path().display());
        for cancelled in [false, true] {
            let run = temp.path().join(format!("cancelled-{cancelled}"));
            std::fs::create_dir(&run).unwrap();
            let (remote, _) = Remote::open(
                temp.path(),
                OsStr::new(&url),
                OsStr::new(&root.to_string()),
                &run,
            )
            .unwrap();
            let repo = Repository::open_bare(&remote.dir).unwrap();
            let (anchor, owner) = remote.raw_blobs.directory_anchor().unwrap();
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let io = crate::file_async::Io::new(false);
                    let raw = crate::raw_blob_async::RawBlobs::from_anchor(
                        anchor,
                        owner,
                        crate::raw_blob::MEMORY_BUDGET,
                        io.clone(),
                    );
                    let (source, initial) = Source::new(repo, remote).unwrap();
                    assert!(initial.is_empty());
                    let mut first = source.blob(oid).await.unwrap();
                    assert_eq!(first.blobs.len(), 1);
                    let mut independent =
                        tokio::time::timeout(Duration::from_secs(2), source.blob(other))
                            .await
                            .expect("an unrelated cold demand waited for a small handoff")
                            .unwrap();
                    while let Some(blob) = independent.next() {
                        raw.adopt(blob).await.unwrap();
                    }
                    drop(independent);
                    let mut duplicate = Box::pin(source.blob(oid));
                    let result =
                        tokio::time::timeout(Duration::from_millis(100), duplicate.as_mut()).await;
                    let waited = result.is_err();
                    if !cancelled {
                        while let Some(blob) = first.next() {
                            raw.adopt(blob).await.unwrap();
                        }
                    }
                    drop(first);
                    let mut second = match result {
                        Ok(result) => result.unwrap(),
                        Err(_) => duplicate.as_mut().await.unwrap(),
                    };
                    drop(duplicate);
                    while let Some(blob) = second.next() {
                        raw.adopt(blob).await.unwrap();
                    }
                    drop(second);
                    let data = raw.read(oid, 0, 100).await.unwrap();
                    assert_eq!(data.as_slice(), bytes);
                    drop(data);
                    if !cancelled {
                        assert!(!io.worker_started());
                    }
                    let remote = source.finish().await.unwrap();
                    assert!(remote.failure().is_none());
                    assert_eq!(remote.raw_blobs.entries(), 2);
                    raw.finish().await.unwrap();
                    Rc::try_unwrap(io).ok().unwrap().finish().await.unwrap();
                    assert!(
                        waited,
                        "a duplicate demand completed before its first payload was published"
                    );
                });
        }
    }
}
