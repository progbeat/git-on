use super::{
    super::{
        attributes::{get_mask, includes_size},
        files::{component, READ},
        protocol::Operation::*,
        xdr::Bitmap,
        Job, REMOTE_READ_WINDOW,
    },
    protocol::{self, Credentials, Rpc, SessionId, MAX_OPS},
    Context, Server, Writer, MIN_CHANNEL_SIZE,
};
use crate::{backend::FileType, filesystem::SourceLookup, snapshot::PendingObject};
use git2::Oid;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};
use tokio::{
    runtime::Handle,
    sync::{mpsc, Notify},
};

type SlotKey = (SessionId, u32);

enum ObjectDemand {
    Final(Oid),
    ParentTree(Oid),
}

struct ParkedCall {
    job: Job,
    slot: SlotKey,
    object: PendingObject,
    recheck_lookup: bool,
}

enum Event {
    Job(Option<Job>),
    Completion,
}

impl Server {
    pub(in crate::backend::nfs) fn serve_parallel(
        &mut self,
        jobs: &mut mpsc::Receiver<Job>,
        stopped: &AtomicBool,
        runtime: &Handle,
        wake: &Arc<Notify>,
    ) {
        let mut parked = Vec::<ParkedCall>::new();
        let mut barrier = None;
        while !stopped.load(Ordering::Relaxed) {
            let mut index = 0;
            while index < parked.len() {
                if parked[index].object.ready() {
                    let call = parked.swap_remove(index);
                    call.object.finish();
                    if call.recheck_lookup {
                        let next = match &call.job {
                            Job::Call(data, _) => self.read_request(data, &parked),
                            Job::Sweep => None,
                        };
                        if let Some((slot, Some(demand))) = next {
                            if let Some((object, recheck_lookup)) = self.start_demand(demand, wake)
                            {
                                parked.push(ParkedCall {
                                    job: call.job,
                                    slot,
                                    object,
                                    recheck_lookup,
                                });
                                continue;
                            }
                        }
                    }
                    call.job.run(self);
                } else {
                    index += 1;
                }
            }
            let event = if parked.is_empty() {
                Event::Job(barrier.take().or_else(|| jobs.blocking_recv()))
            } else if barrier.is_some() || parked.len() == REMOTE_READ_WINDOW {
                runtime.block_on(wake.notified());
                Event::Completion
            } else {
                runtime.block_on(async {
                    tokio::select! {
                        biased;
                        _ = wake.notified() => Event::Completion,
                        job = jobs.recv() => Event::Job(job),
                    }
                })
            };
            let job = match event {
                Event::Job(Some(job)) if !stopped.load(Ordering::Relaxed) => job,
                Event::Completion => continue,
                _ => break,
            };
            match &job {
                Job::Sweep => {
                    // Pending validated slots still hold their clients' leases.
                    let now = Instant::now();
                    for call in &parked {
                        if let Some(session) = self.sessions.get(&call.slot.0) {
                            if let Some(client) = self.clients.get_mut(&session.client) {
                                client.active = now;
                            }
                        }
                    }
                }
                Job::Call(data, _) => match self.read_request(data, &parked) {
                    Some((slot, Some(demand))) => {
                        if let Some((object, recheck_lookup)) = self.start_demand(demand, wake) {
                            parked.push(ParkedCall {
                                job,
                                slot,
                                object,
                                recheck_lookup,
                            });
                            continue;
                        }
                    }
                    None if !parked.is_empty() => {
                        // Keep replays, session teardown and mutations in
                        // order with calls suspended on their object sizes.
                        barrier = Some(job);
                        continue;
                    }
                    _ => (),
                },
            }
            job.run(self);
        }
    }

    fn start_demand(
        &self,
        demand: ObjectDemand,
        wake: &Arc<Notify>,
    ) -> Option<(PendingObject, bool)> {
        let (oid, recheck_lookup) = match demand {
            ObjectDemand::Final(oid) => (oid, false),
            ObjectDemand::ParentTree(oid) => (oid, true),
        };
        let object = self.core.start_remote_demand(oid)?;
        object.notify_when_ready(wake);
        Some((object, recheck_lookup))
    }

    /// Inspect a new request without consuming its sequence ID. Suspended
    /// requests use the normal RPC path when the demanded object is available.
    fn read_request(
        &mut self,
        data: &[u8],
        parked: &[ParkedCall],
    ) -> Option<(SlotKey, Option<ObjectDemand>)> {
        let (_, Rpc::Compound(credentials, request)) = protocol::rpc(data).ok()? else {
            return None;
        };
        let mut call = protocol::compound(request).ok()?;
        if call.minor != 1 || !(2..=MAX_OPS as u32).contains(&call.count) {
            return None;
        }
        let mut next = || {
            let opcode = call.ops.u32().ok()?;
            protocol::operation(opcode, &mut call.ops).ok()
        };
        let Sequence(sequence) = next()? else {
            return None;
        };
        let id = (sequence.session, sequence.slot);
        let session = self.sessions.get(&sequence.session)?;
        let slot = session.slot(&sequence).ok()?;
        // The short attribute calls below fit in the minimum channel size,
        // including the maximum tag and space for each operation's result.
        if parked.iter().any(|call| call.slot == id)
            || sequence.id != slot.sequence.wrapping_add(1)
            || self.clients.get(&session.client)?.uid != credentials.uid
            || session
                .reply_limit(&sequence, data.len(), call.count)
                .ok()?
                < MIN_CHANNEL_SIZE as usize
        {
            return None;
        }
        if parked.is_empty() && !(3..=6).contains(&call.count) {
            return Some((id, None));
        }
        let ops: Vec<_> = (1..call.count).map(|_| next()).collect::<Option<_>>()?;
        call.ops.finish().ok()?;
        if !ops.iter().all(|op| match op {
            Open(open) => open.create.is_none() && open.access & 3 == READ,
            PutFh(_)
            | PutRoot
            | Access(_)
            | GetAttr(_)
            | Lookup(_)
            | GetFh
            | Read(..)
            | ReadDir { .. }
            | Close(_)
            | Save
            | Restore => true,
            _ => false,
        }) {
            return None;
        }
        let oid = match ops.as_slice() {
            [PutFh(handle), ReadDir {
                cookie: 0,
                maxcount,
                ..
            }] if *maxcount >= 16 => self.directory_demand(credentials, handle, None),
            [PutFh(handle), GetAttr(mask), ReadDir {
                cookie: 0,
                maxcount,
                ..
            }] if *maxcount >= 16 => self.directory_demand(credentials, handle, Some(*mask)),
            [PutFh(handle), GetAttr(mask)] => {
                let ino = self.inode_from_handle(handle).ok()?;
                self.attribute_size_demand(ino, *mask)
                    .map(ObjectDemand::Final)
            }
            [PutFh(handle), GetAttr(parent_mask), Lookup(name), GetFh, GetAttr(mask)] => {
                self.lookup_size_demand(credentials, handle, Some(*parent_mask), name, *mask)
            }
            [PutFh(handle), Lookup(name), GetFh, GetAttr(mask)] => {
                self.lookup_size_demand(credentials, handle, None, name, *mask)
            }
            _ => None,
        };
        Some((id, oid))
    }

    fn directory_demand(
        &mut self,
        credentials: Credentials,
        handle: &[u8],
        prior_mask: Option<Bitmap>,
    ) -> Option<ObjectDemand> {
        let ino = self.inode_from_handle(handle).ok()?;
        let oid = self.core.pending_source_tree(ino)?;
        if let Some(mask) = prior_mask {
            self.get_attributes(ino, mask, false, &mut Writer::default())
                .ok()?;
        }
        self.require_access(&Context::new(credentials), ino, libc::R_OK | libc::X_OK)
            .ok()?;
        Some(ObjectDemand::Final(oid))
    }

    fn attribute_size_demand(&self, ino: u64, mask: Bitmap) -> Option<Oid> {
        includes_size(get_mask(mask, false).ok()?)
            .then(|| self.core.pending_source_size(ino))
            .flatten()
    }

    fn lookup_size_demand(
        &mut self,
        credentials: Credentials,
        handle: &[u8],
        parent_mask: Option<Bitmap>,
        name: &[u8],
        mask: Bitmap,
    ) -> Option<ObjectDemand> {
        let parent = self.inode_from_handle(handle).ok()?;
        if self.core.nfs_attributes(parent, false).ok()?.0.kind != FileType::Directory {
            return None;
        }
        if let Some(parent_mask) = parent_mask {
            self.get_attributes(parent, parent_mask, false, &mut Writer::default())
                .ok()?;
        }
        self.require_access(&Context::new(credentials), parent, libc::X_OK)
            .ok()?;
        match self
            .core
            .probe_source_lookup(parent, component(name).ok()?)?
        {
            SourceLookup::Inode(ino) => self
                .attribute_size_demand(ino, mask)
                .map(ObjectDemand::Final),
            SourceLookup::Tree(oid) => Some(ObjectDemand::ParentTree(oid)),
        }
    }
}
