use super::{
    super::{
        attributes::{get_mask, includes_size},
        files::{component, READ},
        protocol::Operation::*,
        xdr::Bitmap,
        Job, QUEUE_DEPTH,
    },
    protocol::{self, Credentials, Rpc, SessionId, MAX_OPS},
    Context, Server, Writer, MIN_CHANNEL_SIZE,
};
use crate::backend::FileType;
use git2::Oid;
use std::{collections::VecDeque, time::Duration};
use tokio::{runtime::Handle, sync::mpsc, time::Instant};

enum ReadRequest {
    Size(Oid),
    DirectoryAttributes,
    Other,
}

impl Server {
    pub(in crate::backend::nfs) fn combine_pending_sizes(
        &mut self,
        first: &[u8],
        waiting: &mut VecDeque<Job>,
        jobs: &mut mpsc::Receiver<Job>,
        runtime: &Handle,
    ) {
        if (waiting.is_empty() && jobs.is_empty()) || !self.core.can_combine_remote_sizes() {
            return;
        }
        let mut slots = Vec::new();
        let Some(ReadRequest::Size(first)) = self.read_request(first, &mut slots) else {
            return;
        };
        let mut wants = vec![first];
        let mut checked = 0;
        let mut unblocked = false;
        let until = Instant::now() + Duration::from_millis(3);
        for _ in 0..(2 * QUEUE_DEPTH - 1) {
            if checked == QUEUE_DEPTH - 1 {
                break;
            }
            if checked == waiting.len() {
                let job = jobs.try_recv().ok().or_else(|| {
                    // A directory-attribute reply may let an independent
                    // lookup reach us. A lone lookup incurs no wait.
                    unblocked
                        .then(|| {
                            runtime.block_on(async {
                                tokio::time::timeout_at(until, jobs.recv())
                                    .await
                                    .ok()
                                    .flatten()
                            })
                        })
                        .flatten()
                });
                let Some(job) = job else { break };
                waiting.push_back(job);
            }
            let Job::Call(data, _) = &waiting[checked] else {
                break;
            };
            match self.read_request(data, &mut slots) {
                Some(ReadRequest::Size(oid)) => {
                    wants.push(oid);
                    checked += 1;
                    unblocked = false;
                }
                Some(ReadRequest::DirectoryAttributes) => {
                    waiting.remove(checked).unwrap().run(self);
                    slots.pop();
                    unblocked = true;
                }
                Some(ReadRequest::Other) => checked += 1,
                None => break,
            }
        }
        if wants.len() > 1 {
            self.core.combine_remote_sizes(&wants);
        }
    }

    /// A valid, independent slot may be inspected without executing its RPC.
    /// Requests that can modify the namespace or session end the lookahead.
    fn read_request(
        &mut self,
        data: &[u8],
        slots: &mut Vec<(SessionId, u32)>,
    ) -> Option<ReadRequest> {
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
        if slots.contains(&id)
            || sequence.id != slot.sequence.wrapping_add(1)
            || self.clients.get(&session.client)?.uid != credentials.uid
            || session
                .reply_limit(&sequence, data.len(), call.count)
                .ok()?
                < MIN_CHANNEL_SIZE as usize
        {
            return None;
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
        slots.push(id);
        let oid = match ops.as_slice() {
            [PutFh(handle), GetAttr(mask)] => {
                let ino = self.inode_from_handle(handle).ok()?;
                let mask = get_mask(*mask, false).ok()?;
                if self.core.protected_source(ino)
                    && self.core.nfs_attributes(ino, false).ok()?.0.kind == FileType::Directory
                {
                    return Some(ReadRequest::DirectoryAttributes);
                }
                self.attribute_size_demand(ino, mask)
            }
            [PutFh(handle), GetAttr(parent_mask), Lookup(name), GetFh, GetAttr(mask)] => {
                self.lookup_size_demand(credentials, handle, *parent_mask, name, *mask)
            }
            _ => None,
        };
        Some(oid.map_or(ReadRequest::Other, ReadRequest::Size))
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
        parent_mask: Bitmap,
        name: &[u8],
        mask: Bitmap,
    ) -> Option<Oid> {
        let parent = self.inode_from_handle(handle).ok()?;
        if self.core.nfs_attributes(parent, false).ok()?.0.kind != FileType::Directory {
            return None;
        }
        self.get_attributes(parent, parent_mask, false, &mut Writer::default())
            .ok()?;
        self.require_access(&Context::new(credentials), parent, libc::X_OK)
            .ok()?;
        let ino = self
            .core
            .lookup_cached_inode(parent, component(name).ok()?)?;
        self.attribute_size_demand(ino, mask)
    }
}
