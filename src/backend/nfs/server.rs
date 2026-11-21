use super::{
    files::{DirectoryView, OpenState},
    locks::LockState,
    protocol::{self, Channel, Credentials, Operation, Rpc, SessionId, StateId, MAX_OPS},
    status::*,
    xdr::{Writer, MAX_RECORD},
};
use crate::filesystem::Overlay;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

mod demand;

pub(super) const LEASE_SECONDS: u32 = 90;
const MAX_CLIENTS: usize = 8;
const MAX_SESSIONS: usize = 4;
const BASE_SLOTS: u32 = 8;
const MIN_CHANNEL_SIZE: u32 = 4096;
const RPC_REPLY_HEADER: usize = 24;

fn fore_slot_limit(response: u32, parallel_reads: bool) -> u32 {
    if parallel_reads {
        // Every slot can retain a complete reply. Preserve the session's
        // original byte bound when the negotiated replies are small.
        let replay_budget = BASE_SLOTS * MAX_RECORD as u32;
        (replay_budget / response).min(super::REMOTE_READ_WINDOW as u32)
    } else {
        BASE_SLOTS
    }
}

struct Client {
    owner: Box<[u8]>,
    verifier: [u8; 8],
    uid: u32,
    next_sequence: u32,
    confirmed: bool,
    reclaim_complete: bool,
    reclaim_filesystem_complete: bool,
    active: Instant,
    create_reply: Option<(u32, Vec<u8>)>,
}

#[derive(Default)]
struct Slot {
    sequence: u32,
    last: Option<Replay>,
}

struct Replay {
    digest: [u8; 32],
    credentials: Credentials,
    reply: Arc<Vec<u8>>,
}

pub(super) struct RpcReply {
    pub(super) header: Vec<u8>,
    pub(super) body: Option<Arc<Vec<u8>>>,
}

struct Session {
    client: u64,
    channel: Channel,
    slots: Vec<Slot>,
}

impl Session {
    fn slot(&self, request: &protocol::Sequence) -> Result<&Slot, Status> {
        let slot = self.slots.get(request.slot as usize).ok_or(BADSLOT)?;
        if request.highest >= self.channel.slots {
            return Err(BAD_HIGH_SLOT);
        }
        Ok(slot)
    }

    fn reply_limit(
        &self,
        request: &protocol::Sequence,
        request_size: usize,
        count: u32,
    ) -> Result<usize, Status> {
        if request_size > self.channel.request as usize {
            return Err(REQ_TOO_BIG);
        }
        if count > self.channel.ops {
            return Err(TOO_MANY_OPS);
        }
        let max_reply = if request.cache_this {
            self.channel.cached as usize
        } else {
            self.channel.response as usize
        };
        // Leave space for the RPC header, the longest tag, SEQUENCE and a later error.
        if max_reply < 1200 {
            return Err(REP_TOO_BIG_TO_CACHE);
        }
        Ok(max_reply)
    }
}

pub(super) struct Server {
    pub core: Overlay,
    pub run: [u8; 16],
    failures: Arc<super::FilesystemFailures>,
    clients: HashMap<u64, Client>,
    sessions: HashMap<SessionId, Session>,
    next_id: u64,
    pub files: HashMap<u64, OpenState>,
    pub locks: HashMap<u64, LockState>,
    pub directories: HashMap<(u64, u64), DirectoryView>,
}

pub(super) struct Context {
    pub client: u64,
    pub credentials: Credentials,
    pub fh: Option<u64>,
    pub saved: Option<u64>,
    pub current_state: Option<StateId>,
    pub saved_state: Option<StateId>,
    pub closed_inodes: Vec<u64>,
}

impl Context {
    fn new(credentials: Credentials) -> Self {
        Self {
            client: 0,
            credentials,
            fh: None,
            saved: None,
            current_state: None,
            saved_state: None,
            closed_inodes: Vec::new(),
        }
    }

    pub fn inode(&self) -> Result<u64, Status> {
        self.fh.ok_or(NOFILEHANDLE)
    }

    pub fn set_inode(&mut self, ino: u64) {
        self.fh = Some(ino);
        self.current_state = None;
    }

    pub fn state_id(&self, state: StateId) -> Result<StateId, Status> {
        if state == StateId::CURRENT {
            self.current_state.ok_or(BAD_STATEID)
        } else {
            Ok(state)
        }
    }
}

pub(super) fn operation_error(opcode: u32, error: Status, w: &mut Writer) {
    w.u32(opcode);
    w.u32(error);
    // SETATTR returns an attributes-set bitmap even on failure.
    if opcode == 34 {
        w.u32(0);
    }
}

impl Server {
    pub(super) fn new(
        mut core: Overlay,
        id: uuid::Uuid,
        failures: Arc<super::FilesystemFailures>,
    ) -> Self {
        core.retain_nfs_inodes();
        Self {
            core,
            run: *id.as_bytes(),
            failures,
            clients: HashMap::new(),
            sessions: HashMap::new(),
            next_id: 1,
            files: HashMap::new(),
            locks: HashMap::new(),
            directories: HashMap::new(),
        }
    }

    pub fn id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn io_error(&self) {
        self.failures.io.store(true, Ordering::Relaxed);
    }

    pub fn writeback_error(&self, error: i32) -> Status {
        if matches!(
            error,
            libc::EIO | libc::ENOSPC | libc::EDQUOT | libc::ENFILE | libc::EMFILE
        ) {
            // Darwin can consume writeback errors before userspace observes them.
            let _ = self.failures.writeback.compare_exchange(
                0,
                error,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
        errno(error)
    }

    fn expire_client(&mut self, id: u64) {
        self.clients.remove(&id);
        self.sessions.retain(|_, session| session.client != id);
        self.drop_client_files(id);
    }

    pub fn sweep(&mut self, now: Instant) {
        let lease = Duration::from_secs(u64::from(LEASE_SECONDS));
        let expired: Vec<_> = self
            .clients
            .iter()
            .filter(|(_, client)| now.duration_since(client.active) >= lease)
            .map(|(&id, _)| id)
            .collect();
        for id in expired {
            self.expire_client(id);
        }
        self.drop_expired_directories(now, lease);
    }

    #[cfg(test)]
    pub fn rpc(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        let RpcReply { mut header, body } = self.rpc_reply(data)?;
        if let Some(body) = body {
            header.extend_from_slice(&body);
        }
        Some(header)
    }

    pub(super) fn rpc_reply(&mut self, data: &[u8]) -> Option<RpcReply> {
        let mut w = Writer::default();
        let mut body = None;
        let call = match protocol::rpc(data) {
            Ok((xid, call)) => {
                w.u32(xid);
                Some(call)
            }
            Err(_) => {
                w.fixed(data.get(..4)?);
                None
            }
        };
        w.u32(1); // REPLY
        match call {
            Some(Rpc::MessageVersion) => {
                for v in [1, 0, 2, 2] {
                    w.u32(v);
                }
            }
            Some(Rpc::Auth(error)) => {
                for v in [1, 1, error] {
                    w.u32(v);
                }
            }
            _ => {
                for v in [0, 0, 0] {
                    w.u32(v); // MSG_ACCEPTED, AUTH_NONE verifier
                }
                match call {
                    Some(Rpc::Null) => w.u32(0),
                    Some(Rpc::Program) => w.u32(1),
                    Some(Rpc::Version) => {
                        for v in [2, 4, 4] {
                            w.u32(v);
                        }
                    }
                    Some(Rpc::Procedure) => w.u32(3),
                    Some(Rpc::Compound(credentials, request)) => {
                        if let Ok(compound) = protocol::compound(request) {
                            w.u32(0);
                            body = Some(self.compound(compound, credentials, request, data.len()));
                        } else {
                            w.u32(4); // GARBAGE_ARGS
                        }
                    }
                    None => w.u32(4),
                    _ => unreachable!(),
                }
            }
        }
        Some(RpcReply {
            header: w.into_bytes(),
            body,
        })
    }

    fn compound(
        &mut self,
        mut request: protocol::Compound<'_>,
        credentials: Credentials,
        raw_request: &[u8],
        request_size: usize,
    ) -> Arc<Vec<u8>> {
        let mut reply = Writer::default();
        reply.u32(OK);
        reply.opaque(request.tag);
        let count_at = reply.len();
        reply.u32(0);

        if request.minor != 1 {
            reply.patch_u32(0, MINOR_MISMATCH);
            return Arc::new(reply.into_bytes());
        }
        if request.count == 0 {
            return Arc::new(reply.into_bytes());
        }
        let mut context = Context::new(credentials);
        let mut budget = MAX_RECORD - RPC_REPLY_HEADER;
        let mut replay_slot = None;
        let mut count = 0;
        for i in 0..request.count.min(MAX_OPS as u32) {
            let Ok(opcode) = request.ops.u32() else {
                reply.patch_u32(0, BADXDR);
                break;
            };
            let operation = protocol::operation(opcode, &mut request.ops);
            let result_at = reply.len();
            reply.u32(opcode);
            reply.u32(OK);
            let mut result_opcode = opcode;

            let status = match operation {
                Err(_) => Err(BADXDR),
                Ok(Operation::Illegal) => {
                    result_opcode = OP_ILLEGAL;
                    Err(OP_ILLEGAL)
                }
                Ok(op) => {
                    if i == 0 && request.count > MAX_OPS as u32 {
                        Err(TOO_MANY_OPS)
                    } else if i + 1 == request.count
                        && !matches!(op, Operation::Unsupported)
                        && request.ops.finish().is_err()
                    {
                        Err(BADXDR)
                    } else {
                        match op {
                            Operation::Sequence(sequence) if i == 0 => {
                                let digest = Sha256::digest(raw_request).into();
                                match self.sequence(
                                    &sequence,
                                    credentials,
                                    digest,
                                    request_size,
                                    request.count,
                                    &mut reply,
                                ) {
                                    Ok(Sequenced::Replay(replay)) => return replay,
                                    Ok(Sequenced::New(client, max_reply)) => {
                                        budget = max_reply - RPC_REPLY_HEADER;
                                        context.client = client;
                                        replay_slot = Some((sequence, digest));
                                        Ok(())
                                    }
                                    Err(status) => Err(status),
                                }
                            }
                            Operation::Sequence(_) => Err(SEQUENCE_POS),
                            op => {
                                // Always leave room to report a subsequent operation's error.
                                let tail = if i + 1 < request.count { 12 } else { 0 };
                                let limit = budget.saturating_sub(reply.len() + tail);
                                self.perform(op, &mut context, request.count, i, limit, &mut reply)
                            }
                        }
                    }
                }
            };

            count += 1;
            if let Err(error) = status {
                if error == IO {
                    self.io_error();
                }
                if error == DENIED {
                    reply.patch_u32(result_at + 4, error);
                } else {
                    reply.truncate(result_at);
                    operation_error(result_opcode, error, &mut reply);
                }
                reply.patch_u32(0, error);
                break;
            }
        }
        for ino in context.closed_inodes {
            self.core.forget_inode(ino, 1);
        }
        reply.patch_u32(count_at, count);
        // The socket and the replay slot share the exact same response body.
        let mut bytes = reply.into_bytes();
        if replay_slot.is_some() && bytes.capacity() > budget {
            bytes.shrink_to_fit();
        }
        let reply = Arc::new(bytes);
        if let Some((sequence, digest)) = replay_slot {
            if let Some(session) = self.sessions.get_mut(&sequence.session) {
                session.slots[sequence.slot as usize] = Slot {
                    sequence: sequence.id,
                    last: Some(Replay {
                        digest,
                        credentials,
                        reply: reply.clone(),
                    }),
                };
                if let Some(client) = self.clients.get_mut(&session.client) {
                    client.active = Instant::now();
                }
            }
        }
        reply
    }

    fn perform(
        &mut self,
        op: Operation<'_>,
        cx: &mut Context,
        count: u32,
        index: u32,
        limit: usize,
        w: &mut Writer,
    ) -> Result<(), Status> {
        use Operation::*;

        let needed = match op {
            Read(..) | ReadDir { .. } | ReadLink => 0,
            Lock { .. } | LockTest(..) => 1056, // A denied lock can include a maximum-length owner.
            GetAttr(_) | TestStateId(_) => 512,
            _ => 128,
        };
        if limit < needed {
            return Err(REP_TOO_BIG);
        }

        match op {
            Bind(..) | Exchange(_) | CreateSession(_) | DestroyClient(_) if count != 1 => {
                return Err(NOT_ONLY_OP);
            }
            DestroySession(_) if index + 1 != count => return Err(NOT_ONLY_OP),
            _ => (),
        }
        match op {
            Bind(id, direction, rdma) => {
                let session = self.sessions.get(&id).ok_or(BADSESSION)?;
                if rdma || !matches!(direction, 1 | 3) {
                    return Err(INVAL);
                }
                self.clients
                    .get_mut(&session.client)
                    .ok_or(BADSESSION)?
                    .active = Instant::now();
                w.fixed(&id);
                w.u32(1); // fore channel
                w.boolean(false);
                Ok(())
            }
            Exchange(exchange) => self.exchange(exchange, cx.credentials, w),
            CreateSession(session) => self.create_session(session, cx.credentials, w),
            DestroySession(id) => {
                let session = self.sessions.get(&id).ok_or(BADSESSION)?;
                self.check_client_credential(session.client, cx.credentials)?;
                self.sessions.remove(&id);
                Ok(())
            }
            DestroyClient(id) => {
                self.check_client_credential(id, cx.credentials)?;
                if self.sessions.values().any(|session| session.client == id)
                    || self.files.values().any(|file| file.client == id)
                {
                    return Err(CLIENTID_BUSY);
                }
                self.expire_client(id);
                Ok(())
            }
            Unsupported => Err(NOTSUPP),
            _ if cx.client == 0 => Err(OP_NOT_IN_SESSION),
            ReclaimComplete(one_fs) => {
                if one_fs {
                    cx.inode()?;
                }
                let client = self.clients.get_mut(&cx.client).ok_or(BADSESSION)?;
                if client.reclaim_complete || (one_fs && client.reclaim_filesystem_complete) {
                    return Err(COMPLETE_ALREADY);
                }
                if one_fs {
                    client.reclaim_filesystem_complete = true;
                } else {
                    client.reclaim_complete = true;
                }
                Ok(())
            }
            op => self.filesystem(op, cx, limit, w),
        }
    }

    fn check_client_credential(&self, client: u64, credentials: Credentials) -> Result<(), Status> {
        let client = self.clients.get(&client).ok_or(STALE_CLIENTID)?;
        if client.uid != credentials.uid {
            return Err(WRONG_CRED);
        }
        Ok(())
    }

    fn exchange(
        &mut self,
        exchange: protocol::Exchange<'_>,
        credentials: Credentials,
        w: &mut Writer,
    ) -> Result<(), Status> {
        const UPDATE_CONFIRMED: u32 = 0x4000_0000;
        const CONFIRMED: u32 = 0x8000_0000;
        const USE_NON_PNFS: u32 = 0x10000;
        if exchange.flags & !(UPDATE_CONFIRMED | 0x0007_0103) != 0 {
            return Err(INVAL);
        }
        if exchange.protection != 0 {
            return Err(NOTSUPP);
        }
        let found = self.clients.iter().find(|(_, client)| {
            client.owner.as_ref() == exchange.owner && client.verifier == exchange.verifier
        });
        let id = if let Some((&id, client)) = found {
            if client.uid != credentials.uid {
                return Err(CLID_INUSE);
            }
            if !client.confirmed && exchange.flags & UPDATE_CONFIRMED != 0 {
                return Err(NOENT);
            }
            id
        } else {
            if exchange.flags & UPDATE_CONFIRMED != 0 {
                return Err(NOENT);
            }
            if self.clients.len() >= MAX_CLIENTS {
                return Err(DELAY);
            }
            let id = self.id();
            self.clients.insert(
                id,
                Client {
                    owner: exchange.owner.into(),
                    verifier: exchange.verifier,
                    uid: credentials.uid,
                    next_sequence: 1,
                    confirmed: false,
                    reclaim_complete: false,
                    reclaim_filesystem_complete: false,
                    active: Instant::now(),
                    create_reply: None,
                },
            );
            id
        };
        let client = self.clients.get_mut(&id).ok_or(SERVERFAULT)?;
        client.active = Instant::now();
        w.u64(id);
        w.u32(client.next_sequence);
        w.u32(USE_NON_PNFS | if client.confirmed { CONFIRMED } else { 0 });
        w.u32(0); // SP4_NONE
        w.u64(0);
        w.opaque(&self.run);
        w.opaque(&self.run);
        w.u32(0); // server implementation ID is optional
        Ok(())
    }

    fn create_session(
        &mut self,
        request: protocol::CreateSession,
        credentials: Credentials,
        w: &mut Writer,
    ) -> Result<(), Status> {
        self.check_client_credential(request.client, credentials)?;
        let client = self.clients.get(&request.client).ok_or(STALE_CLIENTID)?;
        if let Some((seq, reply)) = &client.create_reply {
            if *seq == request.sequence {
                w.fixed(reply);
                return Ok(());
            }
        }
        if client.next_sequence != request.sequence {
            return Err(SEQ_MISORDERED);
        }
        if request.flags & !7 != 0 {
            return Err(INVAL);
        }
        let fore = request.fore;
        if fore.request < MIN_CHANNEL_SIZE
            || fore.response < MIN_CHANNEL_SIZE
            || fore.ops < 2
            || fore.slots == 0
        {
            return Err(TOOSMALL);
        }
        let response = fore.response.min(MAX_RECORD as u32);
        let max_slots = fore_slot_limit(response, self.core.parallel_remote_reads());
        let channel = Channel {
            request: fore.request.min(MAX_RECORD as u32),
            response,
            cached: fore.cached.min(fore.response).min(MAX_RECORD as u32),
            ops: fore.ops.min(MAX_OPS as u32),
            slots: fore.slots.min(max_slots),
        };
        if !client.confirmed {
            let replaced: Vec<_> = self
                .clients
                .iter()
                .filter(|(&id, other)| {
                    id != request.client
                        && other.confirmed
                        && other.uid == client.uid
                        && other.owner == client.owner
                })
                .map(|(&id, _)| id)
                .collect();
            for id in replaced {
                self.expire_client(id);
            }
        }
        if self.sessions.len() >= MAX_SESSIONS {
            return Err(DELAY);
        }
        let mut id = self.run;
        id[8..].copy_from_slice(&self.id().to_be_bytes());
        self.sessions.insert(
            id,
            Session {
                client: request.client,
                channel,
                slots: (0..channel.slots).map(|_| Slot::default()).collect(),
            },
        );
        let start = w.len();
        w.fixed(&id);
        w.u32(request.sequence);
        w.u32(0); // volatile, no callback connection, no RDMA
        channel.encode(w);
        request.back.encode(w);
        let client = self
            .clients
            .get_mut(&request.client)
            .ok_or(STALE_CLIENTID)?;
        client.confirmed = true;
        client.active = Instant::now();
        client.next_sequence = request.sequence.wrapping_add(1);
        client.create_reply = Some((request.sequence, w.bytes()[start..].to_vec()));
        Ok(())
    }

    fn sequence(
        &mut self,
        request: &protocol::Sequence,
        credentials: Credentials,
        digest: [u8; 32],
        request_size: usize,
        count: u32,
        w: &mut Writer,
    ) -> Result<Sequenced, Status> {
        let session = self.sessions.get(&request.session).ok_or(BADSESSION)?;
        let slot = session.slot(request)?;
        if request.id == slot.sequence {
            let prior = slot.last.as_ref().ok_or(SEQ_MISORDERED)?;
            if prior.digest != digest || prior.credentials != credentials {
                return Err(SEQ_FALSE_RETRY);
            }
            self.clients
                .get_mut(&session.client)
                .ok_or(BADSESSION)?
                .active = Instant::now();
            return Ok(Sequenced::Replay(prior.reply.clone()));
        }
        if request.id != slot.sequence.wrapping_add(1) {
            return Err(SEQ_MISORDERED);
        }
        let max_reply = session.reply_limit(request, request_size, count)?;
        self.clients
            .get_mut(&session.client)
            .ok_or(BADSESSION)?
            .active = Instant::now();
        w.fixed(&request.session);
        for value in [
            request.id,
            request.slot,
            session.channel.slots - 1,
            session.channel.slots - 1,
            0,
        ] {
            w.u32(value);
        }
        Ok(Sequenced::New(session.client, max_reply))
    }
}

enum Sequenced {
    New(u64, usize),
    Replay(Arc<Vec<u8>>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fore_slots_expand_within_the_original_replay_budget() {
        let max_reply = MAX_RECORD as u32;
        assert_eq!(fore_slot_limit(max_reply, true), BASE_SLOTS);
        assert_eq!(fore_slot_limit(max_reply / 2, true), 16);
        assert_eq!(fore_slot_limit(max_reply / 4, true), 32);
        assert_eq!(fore_slot_limit(MIN_CHANNEL_SIZE, true), 32);
        assert_eq!(fore_slot_limit(MIN_CHANNEL_SIZE, false), BASE_SLOTS);
    }
}
