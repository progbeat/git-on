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
    Final(Oid, Option<u64>),
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
                            Job::Call(data, _) => {
                                self.read_request(data, parked.iter().map(|call| call.slot))
                            }
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
                Job::Call(data, _) => match self
                    .read_request(data, parked.iter().map(|call| call.slot))
                {
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
        &mut self,
        demand: ObjectDemand,
        wake: &Arc<Notify>,
    ) -> Option<(PendingObject, bool)> {
        let (oid, inode, recheck_lookup) = match demand {
            ObjectDemand::Final(oid, inode) => (oid, inode, false),
            ObjectDemand::ParentTree(oid) => (oid, None, true),
        };
        let object = self.core.start_remote_demand(oid, inode)?;
        object.notify_when_ready(wake);
        Some((object, recheck_lookup))
    }

    /// Inspect a new request without consuming its sequence ID. Suspended
    /// requests use the normal RPC path when the demanded object is available.
    fn read_request(
        &mut self,
        data: &[u8],
        mut parked: impl ExactSizeIterator<Item = SlotKey>,
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
        let idle = parked.len() == 0;
        // The short attribute calls below fit in the minimum channel size,
        // including the maximum tag and space for each operation's result.
        if parked.any(|slot| slot == id)
            || sequence.id != slot.sequence.wrapping_add(1)
            || session
                .reply_limit(&sequence, data.len(), call.count)
                .ok()?
                < MIN_CHANNEL_SIZE as usize
        {
            return None;
        }
        let same_uid = self.clients.get(&session.client)?.uid == credentials.uid;
        if idle && !(3..=6).contains(&call.count) {
            return same_uid.then_some((id, None));
        }
        let ops: Vec<_> = (1..call.count).map(|_| next()).collect::<Option<_>>()?;
        call.ops.finish().ok()?;
        // AUTH_SYS credentials belong to the call. Short attribute and lookup
        // compounds can use an independent slot even under a different UID.
        let demand = match ops.as_slice() {
            [PutFh(handle), ReadDir {
                cookie: 0,
                maxcount,
                ..
            }] if same_uid && *maxcount >= 16 => self.directory_demand(credentials, handle, None),
            [PutFh(handle), GetAttr(mask), ReadDir {
                cookie: 0,
                maxcount,
                ..
            }] if same_uid && *maxcount >= 16 => {
                self.directory_demand(credentials, handle, Some(*mask))
            }
            [PutFh(handle), GetAttr(mask)] => {
                let ino = self.inode_from_handle(handle).ok()?;
                self.attribute_size_demand(ino, *mask)
                    .map(|oid| ObjectDemand::Final(oid, Some(ino)))
            }
            [PutFh(handle), GetAttr(parent_mask), Lookup(name), GetFh, GetAttr(mask)] => {
                self.lookup_size_demand(credentials, handle, Some(*parent_mask), name, *mask)
            }
            [PutFh(handle), Lookup(name), GetFh, GetAttr(mask)] => {
                self.lookup_size_demand(credentials, handle, None, name, *mask)
            }
            _ => {
                if !same_uid
                    || !ops.iter().all(|op| match op {
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
                    })
                {
                    return None;
                }
                None
            }
        };
        // Reserve only the exact object during admission under a different
        // UID. The regular RPC path applies permissions and read-ahead.
        let demand = match demand {
            Some(ObjectDemand::Final(oid, _)) if !same_uid => Some(ObjectDemand::Final(oid, None)),
            demand => demand,
        };
        Some((id, demand))
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
        Some(ObjectDemand::Final(oid, None))
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
                .map(|oid| ObjectDemand::Final(oid, Some(ino))),
            SourceLookup::Tree(oid) => Some(ObjectDemand::ParentTree(oid)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        backend::nfs::{
            status::{ACCESS, OK, SEQ_FALSE_RETRY},
            tests::{call_as, fixture, results, sequence, session_with, xdr},
        },
        policy::WritePolicy,
    };

    #[test]
    fn mixed_uid_attribute_calls_preserve_permissions_and_slot_order() {
        let (_dir, mut server) = fixture(WritePolicy::Untracked);
        let owner = |xid, count, ops: &[u8]| call_as(xid, count, ops, 10, 11, &[]);
        let reader = |xid, count, ops: &[u8]| call_as(xid, count, ops, 20, 21, &[]);
        let session = session_with(&mut server, owner);
        let ino = server.core.lookup_inode(1, "tracked".as_ref()).unwrap();
        let handle = server.filehandle(ino);
        let getattr = xdr(|w| {
            sequence(w, &session, 1, 1, 3);
            w.u32(22); // PUTFH
            w.opaque(&handle);
            w.u32(9); // GETATTR size
            w.u32(1);
            w.u32(1 << 4);
        });
        let request = reader(3, 3, &getattr);
        let parked = || [(session, 0)].into_iter();
        assert!(matches!(
            server.read_request(&request, parked()),
            Some(((id, 1), Some(ObjectDemand::Final(_, None)))) if id == session
        ));
        assert!(server
            .read_request(&request, [(session, 1)].into_iter())
            .is_none());
        let reply = server.rpc(&request).unwrap();
        results(&reply, OK, 3);
        assert!(server.read_request(&request, parked()).is_none());
        assert_eq!(server.rpc(&request).unwrap(), reply);
        results(
            &server.rpc(&owner(4, 3, &getattr)).unwrap(),
            SEQ_FALSE_RETRY,
            1,
        );

        let private = server
            .core
            .create_node(
                1,
                "private".as_ref(),
                FileType::Directory,
                0o700,
                10,
                11,
                Vec::new(),
            )
            .unwrap();
        let lookup = xdr(|w| {
            sequence(w, &session, 2, 1, 3);
            w.u32(22); // PUTFH
            w.opaque(&server.filehandle(private.ino));
            w.u32(15); // LOOKUP
            w.opaque(b"child");
            w.u32(10); // GETFH
            w.u32(9); // GETATTR size
            w.u32(1);
            w.u32(1 << 4);
        });
        let request = reader(5, 5, &lookup);
        assert!(matches!(
            server.read_request(&request, parked()),
            Some((_, None))
        ));
        results(&server.rpc(&request).unwrap(), ACCESS, 3);

        let remove = xdr(|w| {
            sequence(w, &session, 3, 1, 3);
            w.u32(22); // PUTFH
            w.opaque(&server.filehandle(1));
            w.u32(28); // REMOVE
            w.opaque(b"private");
        });
        assert!(server
            .read_request(&reader(6, 3, &remove), parked())
            .is_none());
    }
}
