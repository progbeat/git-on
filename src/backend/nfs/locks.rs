use super::{
    files::next_sequence,
    protocol::{LockOwner, LockRange, Locker, StateId},
    server::{Context, Server},
    status::*,
    xdr::Writer,
};

const END_OF_FILE: u128 = u64::MAX as u128 + 1;

pub(super) struct LockState {
    pub ino: u64,
    pub open: u64,
    client: u64,
    owner: Box<[u8]>,
    sequence: u32,
    ranges: Vec<Range>,
}

#[derive(Clone, Copy)]
struct Range {
    start: u128,
    end: u128,
    write: bool,
}

impl Range {
    fn new(request: LockRange) -> Result<Self, Status> {
        let write = match request.kind {
            1 | 3 => false,
            2 | 4 => true,
            _ => return Err(INVAL),
        };
        if request.length == 0 {
            return Err(BAD_RANGE);
        }
        let start = u128::from(request.offset);
        let end = if request.length == u64::MAX {
            END_OF_FILE
        } else {
            start + u128::from(request.length)
        };
        if end > END_OF_FILE {
            return Err(BAD_RANGE);
        }
        Ok(Self { start, end, write })
    }

    fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }

    fn encode_denied(self, owner: &LockState, w: &mut Writer) {
        w.u64(self.start as u64);
        w.u64(if self.end == END_OF_FILE {
            u64::MAX
        } else {
            (self.end - self.start) as u64
        });
        w.u32(if self.write { 2 } else { 1 });
        w.u64(owner.client);
        w.opaque(&owner.owner);
    }
}

fn replace(ranges: &[Range], range: Range, adding: bool) -> Vec<Range> {
    let mut next = Vec::with_capacity(ranges.len() + 2);
    for &entry in ranges {
        if !entry.overlaps(range) {
            next.push(entry);
            continue;
        }
        if entry.start < range.start {
            next.push(Range {
                end: range.start,
                ..entry
            });
        }
        if entry.end > range.end {
            next.push(Range {
                start: range.end,
                ..entry
            });
        }
    }
    if adding {
        next.push(range);
    }
    next.sort_unstable_by_key(|entry| entry.start);
    let mut merged: Vec<Range> = Vec::with_capacity(next.len());
    for entry in next {
        if let Some(last) = merged
            .last_mut()
            .filter(|last| last.end == entry.start && last.write == entry.write)
        {
            last.end = entry.end;
        } else {
            merged.push(entry);
        }
    }
    merged
}

impl Server {
    pub fn lock_for_state(
        &self,
        cx: &Context,
        ino: u64,
        state: StateId,
    ) -> Result<(u64, &LockState), Status> {
        let state = cx.state_id(state)?;
        let key = self.state_key(state)?;
        let lock = self
            .locks
            .get(&key)
            .filter(|state| state.client == cx.client && state.ino == ino)
            .ok_or(BAD_STATEID)?;
        Self::check_state_sequence(state, lock.sequence)?;
        Ok((key, lock))
    }

    fn conflicts(
        &self,
        client: u64,
        ino: u64,
        owner: &[u8],
        requested: Range,
        w: &mut Writer,
    ) -> Result<(), Status> {
        for lock in self.locks.values() {
            if lock.ino != ino || (lock.client == client && lock.owner.as_ref() == owner) {
                continue;
            }
            if let Some(range) = lock
                .ranges
                .iter()
                .find(|range| range.overlaps(requested) && (range.write || requested.write))
            {
                range.encode_denied(lock, w);
                return Err(DENIED);
            }
        }
        Ok(())
    }

    fn remove_owner_range(
        &mut self,
        ino: u64,
        client: u64,
        owner: &[u8],
        except: Option<u64>,
        range: Range,
    ) {
        for (&key, lock) in &mut self.locks {
            if Some(key) != except
                && lock.ino == ino
                && lock.client == client
                && lock.owner.as_ref() == owner
                && lock.ranges.iter().any(|entry| entry.overlaps(range))
            {
                // Lock ownership spans opens. Only the operated stateid
                // advances; the other stateids lose their overlapping bytes.
                lock.ranges = replace(&lock.ranges, range, false);
            }
        }
    }

    pub fn lock(
        &mut self,
        cx: &mut Context,
        range: LockRange,
        reclaim: bool,
        owner: Locker<'_>,
        w: &mut Writer,
    ) -> Result<(), Status> {
        let ino = cx.inode()?;
        if reclaim {
            return Err(NO_GRACE);
        }
        self.check_file(ino)?;
        let range = Range::new(range)?;
        let (open_key, access, prior, name) = match owner {
            Locker::New(id, owner) => {
                let (open_key, open) = self.open_for_state(cx, ino, id)?;
                let prior = self.locks.iter().find_map(|(&key, lock)| {
                    (lock.open == open_key
                        && lock.client == cx.client
                        && lock.owner.as_ref() == owner.name)
                        .then_some(key)
                });
                (open_key, open.access, prior, owner.name)
            }
            Locker::Existing(id) => {
                let (key, lock) = self.lock_for_state(cx, ino, id)?;
                let open = self.files.get(&lock.open).ok_or(BAD_STATEID)?;
                (lock.open, open.access, Some(key), lock.owner.as_ref())
            }
        };
        if access & (if range.write { 2 } else { 1 }) == 0 {
            return Err(OPENMODE);
        }
        self.conflicts(cx.client, ino, name, range, w)?;
        let owner: Box<[u8]> = name.into();
        self.remove_owner_range(ino, cx.client, &owner, prior, range);
        let (key, sequence) = if let Some(key) = prior {
            let lock = self.locks.get_mut(&key).ok_or(SERVERFAULT)?;
            lock.ranges = replace(&lock.ranges, range, true);
            lock.sequence = next_sequence(lock.sequence);
            (key, lock.sequence)
        } else {
            let key = self.id();
            self.locks.insert(
                key,
                LockState {
                    ino,
                    open: open_key,
                    client: cx.client,
                    owner,
                    sequence: 1,
                    ranges: vec![range],
                },
            );
            (key, 1)
        };
        let state = self.state(key, sequence);
        cx.current_state = Some(state);
        state.encode(w);
        Ok(())
    }

    pub fn lock_test(
        &mut self,
        cx: &Context,
        range: LockRange,
        owner: LockOwner<'_>,
        w: &mut Writer,
    ) -> Result<(), Status> {
        let ino = cx.inode()?;
        self.check_file(ino)?;
        self.conflicts(cx.client, ino, owner.name, Range::new(range)?, w)
    }

    pub fn unlock(
        &mut self,
        cx: &mut Context,
        range: LockRange,
        id: StateId,
        w: &mut Writer,
    ) -> Result<(), Status> {
        let ino = cx.inode()?;
        let (key, lock) = self.lock_for_state(cx, ino, id)?;
        let range = Range::new(range)?;
        let owner = lock.owner.clone();
        self.remove_owner_range(ino, cx.client, &owner, None, range);
        let lock = self.locks.get_mut(&key).ok_or(SERVERFAULT)?;
        lock.sequence = next_sequence(lock.sequence);
        let sequence = lock.sequence;
        let state = self.state(key, sequence);
        cx.current_state = Some(state);
        state.encode(w);
        Ok(())
    }

    pub fn free_lock_state(&mut self, cx: &Context, id: StateId) -> Result<(), Status> {
        let id = cx.state_id(id)?;
        let key = self.state_key(id)?;
        if self
            .files
            .get(&key)
            .is_some_and(|open| open.client == cx.client)
        {
            return Err(LOCKS_HELD);
        }
        let lock = self.locks.get(&key).ok_or(BAD_STATEID)?;
        self.lock_for_state(cx, lock.ino, id)?;
        if !lock.ranges.is_empty() {
            return Err(LOCKS_HELD);
        }
        self.locks.remove(&key);
        Ok(())
    }
}
