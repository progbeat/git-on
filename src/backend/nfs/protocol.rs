//! Wire definitions for the implemented NFSv4.1 operations (RFC 5662).
use super::xdr::{Bitmap, Decode, InvalidXdr, Reader, Writer, MAX_DATA, MAX_RECORD};

pub(super) const MAX_OPS: usize = 64;
pub(super) const MAX_NAME: usize = 255;
pub(super) const MAX_OWNER: usize = 1024;
// NFS link targets are byte strings. The bound is above Darwin's native PATH_MAX.
pub(super) const MAX_LINK: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Credentials {
    pub uid: u32,
    pub gid: u32,
    groups: [u32; 16],
    group_count: usize,
}

impl Credentials {
    fn decode(r: &mut Reader<'_>) -> Decode<Self> {
        r.u32()?; // stamp
        r.opaque(255)?;
        let uid = r.u32()?;
        let gid = r.u32()?;
        let mut groups = [0; 16];
        let group_count = r.count(groups.len())?;
        for group in &mut groups[..group_count] {
            *group = r.u32()?;
        }
        Ok(Self {
            uid,
            gid,
            groups,
            group_count,
        })
    }

    pub fn groups(&self) -> &[u32] {
        &self.groups[..self.group_count]
    }
}

pub(super) enum Rpc<'a> {
    Null,
    Compound(Credentials, &'a [u8]),
    MessageVersion,
    Program,
    Version,
    Procedure,
    Auth(u32),
}

pub(super) fn rpc(data: &[u8]) -> Decode<(u32, Rpc<'_>)> {
    if data.len() > MAX_RECORD {
        return Err(InvalidXdr);
    }
    let mut r = Reader::new(data);
    let xid = r.u32()?;
    if r.u32()? != 0 {
        return Err(InvalidXdr);
    }
    if r.u32()? != 2 {
        return Ok((xid, Rpc::MessageVersion));
    }
    let program = r.u32()?;
    let version = r.u32()?;
    let proc = r.u32()?;
    let auth_flavor = r.u32()?;
    let auth_body = r.opaque(400)?;
    let ver_flavor = r.u32()?;
    let ver_body = r.opaque(400)?;
    if ver_flavor != 0 || !ver_body.is_empty() {
        return Ok((xid, Rpc::Auth(3))); // AUTH_BADVERF
    }
    let credentials = match auth_flavor {
        0 if auth_body.is_empty() => None,
        1 => {
            let mut auth = Reader::new(auth_body);
            match Credentials::decode(&mut auth).and_then(|c| auth.finish().map(|()| c)) {
                Ok(creds) => Some(creds),
                Err(_) => return Ok((xid, Rpc::Auth(1))), // AUTH_BADCRED
            }
        }
        _ => return Ok((xid, Rpc::Auth(1))),
    };
    let call = match (program, version, proc) {
        (p, _, _) if p != 100003 => Rpc::Program,
        (_, v, _) if v != 4 => Rpc::Version,
        (_, _, 0) if r.finish().is_ok() => Rpc::Null,
        (_, _, 0) => return Err(InvalidXdr),
        (_, _, 1) => match credentials {
            Some(creds) => Rpc::Compound(creds, r.remaining()),
            None => Rpc::Auth(5), // AUTH_TOOWEAK
        },
        _ => Rpc::Procedure,
    };
    Ok((xid, call))
}

pub(super) struct Compound<'a> {
    pub tag: &'a [u8],
    pub minor: u32,
    pub count: u32,
    pub ops: Reader<'a>,
}

pub(super) fn compound(data: &[u8]) -> Decode<Compound<'_>> {
    let mut r = Reader::new(data);
    let tag = r.opaque(MAX_OWNER)?;
    let minor = r.u32()?;
    let count = r.u32()?;
    Ok(Compound {
        tag,
        minor,
        count,
        ops: r,
    })
}

#[derive(Clone, Copy)]
pub(super) struct Attributes<'a> {
    pub mask: Bitmap,
    pub data: &'a [u8],
}

impl<'a> Attributes<'a> {
    fn decode(r: &mut Reader<'a>) -> Decode<Self> {
        Ok(Self {
            mask: Bitmap::decode(r)?,
            data: r.opaque(MAX_DATA)?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct StateId {
    pub sequence: u32,
    pub other: [u8; 12],
}

impl StateId {
    pub(super) const ANONYMOUS: Self = Self {
        sequence: 0,
        other: [0; 12],
    };
    pub(super) const CURRENT: Self = Self {
        sequence: 1,
        other: [0; 12],
    };
    pub(super) const READ_BYPASS: Self = Self {
        sequence: u32::MAX,
        other: [255; 12],
    };

    fn decode(r: &mut Reader<'_>) -> Decode<Self> {
        Ok(Self {
            sequence: r.u32()?,
            other: r.fixed()?,
        })
    }

    pub(super) fn encode(self, w: &mut Writer) {
        w.u32(self.sequence);
        w.fixed(&self.other);
    }
}

pub(super) type SessionId = [u8; 16];
pub(super) type Verifier = [u8; 8];

pub(super) struct Sequence {
    pub session: SessionId,
    pub id: u32,
    pub slot: u32,
    pub highest: u32,
    pub cache_this: bool,
}

impl Sequence {
    fn decode(r: &mut Reader<'_>) -> Decode<Self> {
        Ok(Self {
            session: r.fixed()?,
            id: r.u32()?,
            slot: r.u32()?,
            highest: r.u32()?,
            cache_this: r.boolean()?,
        })
    }
}

#[derive(Clone, Copy)]
pub(super) struct Channel {
    pub request: u32,
    pub response: u32,
    pub cached: u32,
    pub ops: u32,
    pub slots: u32,
}

impl Channel {
    fn decode(r: &mut Reader<'_>) -> Decode<Self> {
        r.u32()?;
        let attrs = Self {
            request: r.u32()?,
            response: r.u32()?,
            cached: r.u32()?,
            ops: r.u32()?,
            slots: r.u32()?,
        };
        let rdma = r.count(1)?;
        r.take(4 * rdma)?;
        Ok(attrs)
    }

    pub(super) fn encode(self, w: &mut Writer) {
        for n in [
            0,
            self.request,
            self.response,
            self.cached,
            self.ops,
            self.slots,
            0,
        ] {
            w.u32(n);
        }
    }
}

pub(super) struct CreateSession {
    pub client: u64,
    pub sequence: u32,
    pub flags: u32,
    pub fore: Channel,
    pub back: Channel,
}

impl CreateSession {
    fn decode(r: &mut Reader<'_>) -> Decode<Self> {
        let session = Self {
            client: r.u64()?,
            sequence: r.u32()?,
            flags: r.u32()?,
            fore: Channel::decode(r)?,
            back: Channel::decode(r)?,
        };
        r.u32()?; // callback program
        for _ in 0..r.count(8)? {
            match r.u32()? {
                0 => (),
                1 => {
                    Credentials::decode(r)?;
                }
                6 => {
                    r.u32()?;
                    r.opaque(MAX_OWNER)?;
                    r.opaque(MAX_OWNER)?;
                }
                _ => return Err(InvalidXdr),
            }
        }
        Ok(session)
    }
}

pub(super) struct Exchange<'a> {
    pub owner: &'a [u8],
    pub verifier: Verifier,
    pub flags: u32,
    pub protection: u32,
}

impl<'a> Exchange<'a> {
    fn decode(r: &mut Reader<'a>) -> Decode<Self> {
        let exchange = Self {
            verifier: r.fixed()?,
            owner: r.opaque(MAX_OWNER)?,
            flags: r.u32()?,
            protection: r.u32()?,
        };
        match exchange.protection {
            0 => (),
            1 | 2 => {
                Bitmap::decode(r)?;
                Bitmap::decode(r)?;
                if exchange.protection == 2 {
                    for _ in 0..r.count(16)? {
                        r.opaque(MAX_OWNER)?;
                    }
                    for _ in 0..r.count(16)? {
                        r.opaque(MAX_OWNER)?;
                    }
                    r.take(8)?;
                }
            }
            _ => return Err(InvalidXdr),
        }
        if r.count(1)? != 0 {
            r.opaque(MAX_OWNER)?;
            r.opaque(MAX_OWNER)?;
            r.take(12)?;
        }
        Ok(exchange)
    }
}

pub(super) enum CreateHow<'a> {
    Unchecked(Attributes<'a>),
    Guarded(Attributes<'a>),
    Exclusive(Verifier, Option<Attributes<'a>>),
}

pub(super) enum Claim<'a> {
    Name(&'a [u8]),
    Handle,
    Reclaim,
    Delegated,
}

pub(super) struct Open<'a> {
    pub access: u32,
    pub deny: u32,
    pub owner: &'a [u8],
    pub create: Option<CreateHow<'a>>,
    pub claim: Claim<'a>,
}

impl<'a> Open<'a> {
    fn decode(r: &mut Reader<'a>) -> Decode<Self> {
        r.u32()?; // open-owner seqid is obsolete with sessions
        let access = r.u32()?;
        let deny = r.u32()?;
        r.u64()?; // session supplies the client ID
        let owner = r.opaque(MAX_OWNER)?;
        let create = match r.u32()? {
            0 => None,
            1 => Some(match r.u32()? {
                0 => CreateHow::Unchecked(Attributes::decode(r)?),
                1 => CreateHow::Guarded(Attributes::decode(r)?),
                2 => CreateHow::Exclusive(r.fixed()?, None),
                3 => CreateHow::Exclusive(r.fixed()?, Some(Attributes::decode(r)?)),
                _ => return Err(InvalidXdr),
            }),
            _ => return Err(InvalidXdr),
        };
        let claim = match r.u32()? {
            0 => Claim::Name(name(r)?),
            1 => {
                r.u32()?;
                Claim::Reclaim
            }
            2 => {
                StateId::decode(r)?;
                name(r)?;
                Claim::Delegated
            }
            3 => {
                name(r)?;
                Claim::Delegated
            }
            4 => Claim::Handle,
            5 => {
                StateId::decode(r)?;
                Claim::Delegated
            }
            6 => Claim::Delegated,
            _ => return Err(InvalidXdr),
        };
        Ok(Self {
            access,
            deny,
            owner,
            create,
            claim,
        })
    }
}

#[derive(Clone, Copy)]
pub(super) struct LockOwner<'a> {
    pub name: &'a [u8],
}

impl<'a> LockOwner<'a> {
    fn decode(r: &mut Reader<'a>) -> Decode<Self> {
        r.u64()?; // client identity comes from the session
        Ok(Self {
            name: r.opaque(MAX_OWNER)?,
        })
    }
}

pub(super) enum Locker<'a> {
    New(StateId, LockOwner<'a>),
    Existing(StateId),
}

pub(super) struct LockRange {
    pub kind: u32,
    pub offset: u64,
    pub length: u64,
}

pub(super) enum Operation<'a> {
    Access(u32),
    Close(StateId),
    Commit,
    Create {
        kind: u32,
        target: &'a [u8],
        name: &'a [u8],
        attrs: Attributes<'a>,
    },
    GetAttr(Bitmap),
    GetFh,
    Link(&'a [u8]),
    Lock {
        range: LockRange,
        reclaim: bool,
        owner: Locker<'a>,
    },
    LockTest(LockRange, LockOwner<'a>),
    Unlock(LockRange, StateId),
    Lookup(&'a [u8]),
    LookupParent,
    Verify(Attributes<'a>, bool),
    Open(Open<'a>),
    Downgrade(StateId, u32, u32),
    PutFh(&'a [u8]),
    PutRoot,
    Read(StateId, u64, u32),
    ReadDir {
        cookie: u64,
        verifier: Verifier,
        dircount: u32,
        maxcount: u32,
        mask: Bitmap,
    },
    ReadLink,
    Remove(&'a [u8]),
    Rename(&'a [u8], &'a [u8]),
    Restore,
    Save,
    SecInfo(&'a [u8]),
    SetAttr(StateId, Attributes<'a>),
    Write(StateId, u64, u32, &'a [u8]),
    Bind(SessionId, u32, bool),
    Exchange(Exchange<'a>),
    CreateSession(CreateSession),
    DestroySession(SessionId),
    FreeStateId(StateId),
    SecInfoNoName(u32),
    Sequence(Sequence),
    TestStateId(Vec<StateId>),
    DestroyClient(u64),
    ReclaimComplete(bool),
    Unsupported,
    Illegal,
}

fn name<'a>(r: &mut Reader<'a>) -> Decode<&'a [u8]> {
    // Decode enough to distinguish overlong names from malformed XDR.
    r.opaque(MAX_LINK)
}

fn lock_range(r: &mut Reader<'_>, kind: u32) -> Decode<LockRange> {
    Ok(LockRange {
        kind,
        offset: r.u64()?,
        length: r.u64()?,
    })
}

pub(super) fn operation<'a>(opcode: u32, r: &mut Reader<'a>) -> Decode<Operation<'a>> {
    use Operation::*;
    Ok(match opcode {
        3 => Access(r.u32()?),
        4 => {
            r.u32()?;
            Close(StateId::decode(r)?)
        }
        5 => {
            r.u64()?;
            r.u32()?;
            Commit
        }
        6 => {
            let kind = r.u32()?;
            let target = match kind {
                5 => r.opaque(MAX_LINK)?,
                3 | 4 => {
                    r.take(8)?;
                    &[]
                }
                _ => &[],
            };
            Create {
                kind,
                target,
                name: name(r)?,
                attrs: Attributes::decode(r)?,
            }
        }
        9 => GetAttr(Bitmap::decode(r)?),
        10 => GetFh,
        11 => Link(name(r)?),
        12 => {
            let kind = r.u32()?;
            let reclaim = r.boolean()?;
            let range = lock_range(r, kind)?;
            let owner = if r.boolean()? {
                r.u32()?;
                let open = StateId::decode(r)?;
                r.u32()?;
                Locker::New(open, LockOwner::decode(r)?)
            } else {
                let lock = StateId::decode(r)?;
                r.u32()?;
                Locker::Existing(lock)
            };
            Lock {
                range,
                reclaim,
                owner,
            }
        }
        13 => {
            let kind = r.u32()?;
            LockTest(lock_range(r, kind)?, LockOwner::decode(r)?)
        }
        14 => {
            let kind = r.u32()?;
            r.u32()?;
            let stateid = StateId::decode(r)?;
            Unlock(lock_range(r, kind)?, stateid)
        }
        15 => Lookup(name(r)?),
        16 => LookupParent,
        17 | 37 => Verify(Attributes::decode(r)?, opcode == 17),
        18 => Open(self::Open::decode(r)?),
        21 => {
            let id = StateId::decode(r)?;
            r.u32()?;
            Downgrade(id, r.u32()?, r.u32()?)
        }
        22 => PutFh(r.opaque(64)?),
        23 | 24 => PutRoot,
        25 => Read(StateId::decode(r)?, r.u64()?, r.u32()?),
        26 => ReadDir {
            cookie: r.u64()?,
            verifier: r.fixed()?,
            dircount: r.u32()?,
            maxcount: r.u32()?,
            mask: Bitmap::decode(r)?,
        },
        27 => ReadLink,
        28 => Remove(name(r)?),
        29 => Rename(name(r)?, name(r)?),
        31 => Restore,
        32 => Save,
        33 => SecInfo(name(r)?),
        34 => SetAttr(StateId::decode(r)?, Attributes::decode(r)?),
        38 => Write(StateId::decode(r)?, r.u64()?, r.u32()?, r.opaque(MAX_DATA)?),
        41 => Bind(r.fixed()?, r.u32()?, r.boolean()?),
        42 => Exchange(self::Exchange::decode(r)?),
        43 => CreateSession(self::CreateSession::decode(r)?),
        44 => DestroySession(r.fixed()?),
        45 => FreeStateId(StateId::decode(r)?),
        52 => SecInfoNoName(r.u32()?),
        53 => Sequence(self::Sequence::decode(r)?),
        55 => {
            let count = r.count(MAX_OPS)?;
            let mut ids = Vec::with_capacity(count);
            for _ in 0..count {
                ids.push(StateId::decode(r)?);
            }
            TestStateId(ids)
        }
        57 => DestroyClient(r.u64()?),
        58 => ReclaimComplete(r.boolean()?),
        7 | 8 | 19 | 20 | 30 | 35 | 36 | 39 | 40 | 46..=51 | 54 | 56 => Unsupported,
        _ => Illegal,
    })
}
