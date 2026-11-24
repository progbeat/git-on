//! One request queue for HTTP/2, native HTTPS helpers and owned SSH channels.
use crate::{
    object, pack_stream,
    raw_blob::{DemandLookup, RawBlobs, Transfer},
    transport::{Connection, Session, StartingSession},
};
use anyhow::{bail, Context, Result};
use git2::Oid;
use std::{
    collections::{HashMap, VecDeque},
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Condvar, Mutex, OnceLock, Weak,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const CONNECTIONS: usize = 16;
const INITIAL_CONNECTIONS: usize = 4;
const MAX_WANTS: usize = 64;
const READ_AHEAD: usize = 4;
const MAX_READ_AHEAD: usize = 256;
const MAX_UNCONSUMED_PREDICTIONS: usize = 512;
const MIN_PREDICTION_AGE: usize = 8;
const MAX_FORECAST_SCORE: u8 = 24;
const REPLY_SAMPLES: u8 = 8;
pub(crate) const SLOW_REPLY: Duration = Duration::from_millis(64);
const BATCH_TARGET: usize = 4;
const BATCH_WINDOW: Duration = Duration::from_millis(2);
const DENSE_SCORE: u8 = 2;
const FORECAST_MISSES: u8 = 4;
const DENSE_BATCH_TARGET: usize = 24;
const DENSE_BATCH_WINDOW: Duration = Duration::from_millis(12);

pub(crate) enum Acquired {
    Stored,
    ValidatedExtra,
    Object(pack_stream::VerifiedObject),
}

pub(crate) struct Batch {
    pub(crate) oids: Vec<Oid>,
    succeeded: bool,
    from_validated_extra: bool,
    result: Mutex<Option<Result<Acquired>>>,
}

impl Batch {
    fn new(oids: Vec<Oid>, result: Result<Acquired>) -> Arc<Self> {
        Arc::new(Self {
            oids,
            succeeded: result.is_ok(),
            from_validated_extra: matches!(&result, Ok(Acquired::ValidatedExtra)),
            result: Mutex::new(Some(result)),
        })
    }

    pub(crate) fn take(&self) -> Option<Result<Acquired>> {
        self.result.lock().unwrap().take()
    }

    pub(crate) fn requires_native_preparation(&self) -> bool {
        match &*self.result.lock().unwrap() {
            Some(Err(error)) => {
                error.is::<crate::http2::NativeAuthentication>()
                    || error.is::<crate::http2::Http1Required>()
            }
            _ => false,
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct Demand(Arc<Flight>);

#[derive(Default)]
struct Flight {
    state: Mutex<FlightState>,
    ready: Condvar,
    // This is changed only while the queue is locked. Workers may observe
    // demand joining a speculative request after it has been submitted.
    required: AtomicBool,
}

#[derive(Default)]
struct FlightState {
    batch: Option<Arc<Batch>>,
    counted: bool,
    #[cfg(target_os = "macos")]
    notify: Option<Arc<tokio::sync::Notify>>,
}

impl Demand {
    fn complete(&self, batch: Arc<Batch>) {
        self.complete_with(batch, |_| {});
    }

    fn complete_with(&self, batch: Arc<Batch>, record: impl FnOnce(&mut FlightState)) {
        let mut state = self.0.state.lock().unwrap();
        record(&mut state);
        if state.batch.is_some() {
            return;
        }
        state.batch = Some(batch);
        self.0.ready.notify_all();
        #[cfg(target_os = "macos")]
        if let Some(notify) = state.notify.take() {
            notify.notify_one();
        }
    }

    pub(crate) fn wait(&self) -> Arc<Batch> {
        let state = self.0.state.lock().unwrap();
        let state = self
            .0
            .ready
            .wait_while(state, |state| state.batch.is_none())
            .unwrap();
        state.batch.as_ref().unwrap().clone()
    }

    pub(crate) fn ready(&self) -> bool {
        self.0.state.lock().unwrap().batch.is_some()
    }

    fn required(&self) -> bool {
        self.0.required.load(Ordering::Relaxed)
    }

    fn needs_exact_pack(&self, oid: Oid, cache: &RawBlobs, transfer: &Arc<Transfer>) -> bool {
        let state = self.0.state.lock().unwrap();
        if !state
            .batch
            .as_ref()
            .is_some_and(|batch| batch.from_validated_extra)
        {
            return true;
        }
        // A reader can use a cached object without joining this transfer.
        let delivered =
            self.required() && state.batch.as_ref().is_some_and(|batch| batch.succeeded);
        drop(state);
        cache.was_served(oid, transfer, delivered)
    }

    fn delivered(&self) -> bool {
        self.required()
            && self
                .0
                .state
                .lock()
                .unwrap()
                .batch
                .as_ref()
                .is_some_and(|batch| batch.succeeded)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn notify_when_ready(&self, notify: &Arc<tokio::sync::Notify>) {
        let mut state = self.0.state.lock().unwrap();
        if state.batch.is_some() {
            notify.notify_one();
        } else {
            state.notify = Some(notify.clone());
        }
    }
}

struct QueuedDemand {
    oid: Oid,
    demand: Demand,
    queued_at: Instant,
    prefetch_width: usize,
    immediate: bool,
}

struct Work {
    items: Vec<QueuedDemand>,
    readers: Arc<[(Oid, Demand)]>,
    extras: Arc<Mutex<Option<Vec<QueuedDemand>>>>,
    // A different pack can satisfy an exact request while it is in flight.
    // Attribute early reads to the transfer whose bytes actually served them.
    transfer: Arc<Transfer>,
    speculative: bool,
}

#[derive(Clone)]
struct ActivePack {
    readers: Arc<[(Oid, Demand)]>,
    transfer: Arc<Transfer>,
    speculative: bool,
}

impl ActivePack {
    fn supplied_data(&self, scratch: &RawBlobs) -> bool {
        self.transfer.served()
            || self.readers.iter().any(|(oid, demand)| {
                let delivered = demand.delivered();
                scratch.was_served(*oid, &self.transfer, delivered)
                    || (!self.speculative
                        && delivered
                        && demand.needs_exact_pack(*oid, scratch, &self.transfer))
            })
    }
}

impl Work {
    fn take(queue: &mut VecDeque<QueuedDemand>, speculative: bool) -> Option<Self> {
        // Cache publication can satisfy a demand while it is still queued.
        let first = loop {
            let item = queue.pop_front()?;
            if !item.demand.ready() {
                break item;
            }
        };
        let limit = if speculative {
            first.prefetch_width
        } else {
            MAX_WANTS
        };
        let mut items = Vec::with_capacity((queue.len() + 1).min(limit));
        items.push(first);
        while items.len() < limit {
            let Some(item) = queue.pop_front() else { break };
            if !item.demand.ready() {
                items.push(item);
            }
        }
        let readers = items
            .iter()
            .map(|item| (item.oid, item.demand.clone()))
            .collect();
        Some(Self {
            items,
            readers,
            extras: Arc::new(Mutex::new(Some(Vec::new()))),
            transfer: Arc::default(),
            speculative,
        })
    }

    fn wants(&self) -> Vec<Oid> {
        self.items.iter().map(|item| item.oid).collect()
    }

    fn id(&self) -> usize {
        Arc::as_ptr(&self.transfer) as usize
    }

    fn active_pack(&self) -> ActivePack {
        ActivePack {
            readers: self.readers.clone(),
            transfer: self.transfer.clone(),
            speculative: self.speculative,
        }
    }
}

enum ConnectionMode {
    Probing(u8),
    Standard,
    Wide,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum SshChannel {
    #[default]
    Unopened,
    Ready(u32),
    Spent,
    WaitingForStandby,
    Unavailable,
}

impl SshChannel {
    fn is_ready(self) -> bool {
        matches!(self, Self::Ready(_))
    }

    fn has_negotiated(self) -> bool {
        matches!(self, Self::Ready(_) | Self::Spent | Self::WaitingForStandby)
    }

    fn is_unavailable(self) -> bool {
        matches!(self, Self::WaitingForStandby | Self::Unavailable)
    }

    fn is_open(self) -> bool {
        matches!(
            self,
            Self::Ready(pid) if crate::transport::child_exited(pid).is_ok_and(|exited| !exited)
        )
    }
}

impl Default for ConnectionMode {
    fn default() -> Self {
        Self::Probing(0)
    }
}

impl ConnectionMode {
    fn record_reply(&mut self, elapsed: Duration) {
        if let Self::Probing(samples) = self {
            *self = if elapsed < SLOW_REPLY {
                Self::Standard
            } else if *samples + 1 == REPLY_SAMPLES {
                Self::Wide
            } else {
                Self::Probing(*samples + 1)
            };
        }
    }

    fn channels(&self) -> usize {
        if matches!(self, Self::Wide) {
            CONNECTIONS
        } else {
            INITIAL_CONNECTIONS
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
enum MountReads {
    #[default]
    Preparing,
    Lookup,
    Enumeration,
}

struct SshOpenAttempt {
    speculative: bool,
    pending_standbys: usize,
}

#[derive(Default)]
struct Queue {
    waiting: VecDeque<QueuedDemand>,
    ahead: VecDeque<QueuedDemand>,
    pending: HashMap<Oid, Demand>,
    active: HashMap<usize, ActivePack>,
    predicted: HashMap<Oid, u64>,
    forecast_step: u64,
    forecast_score: u8,
    forecast_misses: u8,
    forecast_expanded: bool,
    busy: [bool; CONNECTIONS],
    ssh_exit_checks: Option<[bool; CONNECTIONS]>,
    mount_reads: MountReads,
    ssh_channels: [SshChannel; CONNECTIONS],
    late_failure: Arc<OnceLock<Arc<str>>>,
    initial_channels: usize,
    starting: [bool; CONNECTIONS],
    opening_ssh: Option<SshOpenAttempt>,
    native_ceiling: Option<usize>,
    failure: Option<String>,
    mode: ConnectionMode,
}

impl Queue {
    fn record_channel_failure(
        &mut self,
        used: bool,
        completed: &mut Vec<ActivePack>,
        error: &anyhow::Error,
        scratch: &RawBlobs,
    ) {
        if used || completed.iter().any(|pack| pack.supplied_data(scratch)) {
            self.failure.get_or_insert_with(|| format!("{error:#}"));
        } else if !completed.is_empty() {
            // Cached extras may have their first reader after the channel
            // has closed. Let a reader report that failure directly, without
            // making unrelated demands revisit the channel's completed work.
            let message: Arc<str> = format!("{error:#}").into();
            for pack in completed.drain(..) {
                pack.transfer
                    .report_failure_on_use(message.clone(), self.late_failure.clone());
            }
        }
    }

    fn recorded_failure(&self) -> Option<&str> {
        self.failure
            .as_deref()
            .or_else(|| self.late_failure.get().map(AsRef::as_ref))
    }

    fn insert(&mut self, oid: Oid) -> QueuedDemand {
        let demand = Demand::default();
        self.pending.insert(oid, demand.clone());
        QueuedDemand {
            oid,
            demand,
            queued_at: Instant::now(),
            prefetch_width: READ_AHEAD,
            immediate: false,
        }
    }

    fn forecast_width(&mut self, anchor: Oid) -> usize {
        self.forecast_step = self.forecast_step.saturating_add(1);
        if self.predicted.remove(&anchor).is_some() {
            self.forecast_score = (self.forecast_score + 1).min(MAX_FORECAST_SCORE);
            self.forecast_misses = 0;
            // On a slow source, overlapping reads reinforce the first hit.
            if self.forecast_score == 1
                && matches!(
                    self.mode,
                    ConnectionMode::Probing(1..) | ConnectionMode::Wide
                )
                && self
                    .pending
                    .values()
                    .filter(|demand| demand.required() && !demand.ready())
                    .nth(1)
                    .is_some()
            {
                self.forecast_score = DENSE_SCORE;
            }
        } else if self.forecast_score != 0 {
            self.forecast_misses += 1;
            if self.forecast_misses == FORECAST_MISSES {
                self.forecast_score -= 1;
                self.forecast_misses = 0;
            }
        }
        if !self.dense() {
            return READ_AHEAD;
        }
        self.forecast_expanded = true;
        match self.forecast_score {
            2..=3 => 64,
            4..=5 => 128,
            _ => MAX_READ_AHEAD,
        }
    }

    fn dense(&self) -> bool {
        self.forecast_score >= DENSE_SCORE
    }

    fn prediction_room(&mut self, width: usize) -> usize {
        let capacity = (4 * width).min(MAX_UNCONSUMED_PREDICTIONS);
        if self.predicted.len() >= capacity
            && (self.forecast_expanded || !matches!(self.mode, ConnectionMode::Standard))
        {
            // Completed guesses from an earlier part of a scan can occupy
            // the whole window while callers move through another subtree.
            // Give their slots back without dropping a flight in progress.
            let oldest = self
                .forecast_step
                .saturating_sub(width.max(MIN_PREDICTION_AGE) as u64);
            self.predicted
                .retain(|oid, &mut step| step > oldest || self.pending.contains_key(oid));
        }
        capacity.saturating_sub(self.predicted.len()).min(width)
    }

    fn foreground(&mut self, oid: Oid) -> Demand {
        if let Some(demand) = self.pending.get(&oid).cloned() {
            if !demand.0.required.swap(true, Ordering::Relaxed) {
                if let Some(index) = self.ahead.iter().position(|candidate| candidate.oid == oid) {
                    self.waiting.push_back(self.ahead.remove(index).unwrap());
                }
            }
            return demand;
        }
        let entry = self.insert(oid);
        entry.demand.0.required.store(true, Ordering::Relaxed);
        let demand = entry.demand.clone();
        self.waiting.push_back(entry);
        demand
    }

    fn batch_wait(&self, now: Instant, transfer_active: bool) -> Option<Duration> {
        if !transfer_active {
            return None;
        }
        let (first, window) = if self.dense() {
            if self.waiting.len().max(self.ahead.len()) >= DENSE_BATCH_TARGET {
                return None;
            }
            let first = self
                .waiting
                .front()
                .into_iter()
                .chain(self.ahead.front())
                .map(|item| item.queued_at)
                .min()?;
            let window = if matches!(self.mode, ConnectionMode::Standard) {
                BATCH_WINDOW
            } else {
                DENSE_BATCH_WINDOW
            };
            (first, window)
        } else if matches!(self.mode, ConnectionMode::Wide) && self.waiting.len() < BATCH_TARGET {
            (self.waiting.front()?.queued_at, BATCH_WINDOW)
        } else {
            return None;
        };
        window
            .checked_sub(now.duration_since(first))
            .filter(|remaining| !remaining.is_zero())
    }

    fn http2_batch_wait(&self, now: Instant, transfer_active: bool) -> Option<Duration> {
        // The next pathname operation can depend on the contents of this tree.
        if self.waiting.iter().any(|item| item.immediate) {
            return None;
        }
        self.batch_wait(now, transfer_active)
    }

    fn wait_for_http2_event(&self, delay: Option<Duration>, transfer_active: bool) -> bool {
        if delay.is_some() {
            transfer_active || !matches!(self.mode, ConnectionMode::Standard)
        } else {
            self.waiting.is_empty()
        }
    }

    fn native_work(&mut self, index: usize, needs_ssh_channel: bool) -> Option<NativeWork> {
        for demands in [&mut self.waiting, &mut self.ahead] {
            while demands.front().is_some_and(|item| item.demand.ready()) {
                demands.pop_front();
            }
        }
        let speculative = self.waiting.is_empty();
        let ready_ssh = self.ssh_channels[index].is_ready();
        let reconnecting = self.ssh_channels[index] == SshChannel::Spent;
        let ready_for_exact = !speculative && !needs_ssh_channel;
        let ceiling = self.native_ceiling.unwrap_or(CONNECTIONS);
        let channels = if matches!(self.mode, ConnectionMode::Probing(_)) && self.dense() {
            CONNECTIONS
        } else {
            self.mode.channels()
        }
        .min(ceiling);
        let ready_channels = if speculative { channels } else { ceiling };
        // An exact read can reuse a released slot above the opening limit.
        // Probing an untried slot waits for active packs and startup attempts.
        let recovering = needs_ssh_channel
            && !speculative
            && (0..channels).all(|slot| {
                self.ssh_channels[slot].is_unavailable() || reconnecting && self.starting[slot]
            })
            && (reconnecting
                || !self.busy.contains(&true) && !self.starting[..ceiling].contains(&true));
        let eligible = if !speculative && (ready_ssh || recovering) {
            ceiling
        } else if ready_for_exact {
            channels.max(self.initial_channels).min(ceiling)
        } else {
            channels
        };
        let prefer_ready_ssh = !speculative
            || self
                .ssh_channels
                .iter()
                .any(|channel| *channel == SshChannel::Spent || channel.is_unavailable());
        let can_skip_starting = ready_for_exact || reconnecting && !speculative;
        // A prepared or released channel can serve an exact read while an
        // earlier standby is negotiating.
        if index >= eligible
            || needs_ssh_channel
                && self.ssh_channels[index].is_unavailable()
                && self.failure.is_none()
            || (0..index).any(|earlier| {
                let channel = self.ssh_channels[earlier];
                !(self.busy[earlier]
                    || can_skip_starting && self.starting[earlier]
                    || channel.is_unavailable()
                    || recovering && reconnecting && channel == SshChannel::Unopened
                    || ready_ssh && prefer_ready_ssh && !channel.is_ready())
            })
        {
            return None;
        }
        if needs_ssh_channel
            && prefer_ready_ssh
            && self.failure.is_none()
            && self.has_idle_ssh(index, ready_channels)
        {
            return None;
        }
        let pending = if speculative {
            let active = self.busy.iter().filter(|busy| **busy).count();
            if active + 1 >= channels {
                return None;
            }
            &mut self.ahead
        } else {
            &mut self.waiting
        };
        let work = if needs_ssh_channel && self.failure.is_none() {
            if pending.is_empty()
                || self.opening_ssh.is_some()
                || speculative && index >= INITIAL_CONNECTIONS
            {
                return None;
            }
            // A new channel can take several round trips. Keep the queued
            // objects available while an already open channel might free up.
            self.opening_ssh = Some(SshOpenAttempt {
                speculative,
                pending_standbys: if reconnecting {
                    self.pending_standbys()
                } else {
                    0
                },
            });
            NativeWork::Connect
        } else {
            let work = Work::take(pending, speculative)?;
            // A slow sequential read can use a channel for its final response
            // while another negotiated channel stays eligible for a new read.
            let last_request = self.mount_reads == MountReads::Lookup
                && ready_ssh
                && !speculative
                && !self.dense()
                && matches!(self.mode, ConnectionMode::Probing(1..))
                && work.items.len() == 1
                && self.ahead.is_empty()
                && self.has_idle_ssh(index, ready_channels);
            if last_request {
                // Claim the lease under the queue lock. Concurrent requests
                // must see this channel as spent before another can close.
                self.ssh_channels[index] = SshChannel::Spent;
            }
            NativeWork::Fetch {
                work,
                failure: self.failure.clone(),
                last_request,
            }
        };
        self.busy[index] = true;
        Some(work)
    }

    fn has_idle_ssh(&self, index: usize, limit: usize) -> bool {
        (0..limit).any(|other| {
            // An idle channel's worker still owns its unreaped process.
            other != index && !self.busy[other] && self.ssh_channels[other].is_open()
        })
    }

    fn pending_standbys(&self) -> usize {
        self.starting.iter().filter(|&&pending| pending).count()
    }

    fn finished_ssh_standby(&mut self, index: usize) {
        self.starting[index] = false;
        for channel in &mut self.ssh_channels {
            if *channel == SshChannel::WaitingForStandby {
                *channel = SshChannel::Spent;
            }
        }
    }

    // Return true when the pool must stop on cancellation.
    fn rejected_ssh_channel(&mut self, index: usize, error: anyhow::Error) -> bool {
        let attempt = self.opening_ssh.take().expect("SSH connection in progress");
        if object::is_cancelled(&error) {
            self.cancel_queued();
            return true;
        }
        if self.ssh_exit_checks.is_some() {
            return false;
        }
        if attempt.pending_standbys != 0 {
            // A negotiating standby can temporarily occupy the available
            // slot. Retry after a standby finishes, including when it
            // finished before this refusal reached the queue.
            self.ssh_channels[index] = if self.pending_standbys() < attempt.pending_standbys {
                SshChannel::Spent
            } else {
                SshChannel::WaitingForStandby
            };
            return false;
        }
        let ceiling = self.native_ceiling.unwrap_or(CONNECTIONS);
        // A failed replacement does not describe the server's channel limit.
        // Keep higher lanes that can serve work or still reopen a session.
        if index != 0
            && self.ssh_channels[index] == SshChannel::Unopened
            && !self.ssh_channels[index + 1..ceiling]
                .iter()
                .any(|channel| channel.has_negotiated())
            && !self.starting[index + 1..ceiling].contains(&true)
        {
            self.native_ceiling = Some(index);
            if index == 1 {
                self.discard_prefetch();
            }
        } else {
            self.ssh_channels[index] = SshChannel::Unavailable;
        }
        let usable = self.native_ceiling.unwrap_or(CONNECTIONS);
        // The refusal may have arrived while another session was closing.
        // A previously negotiated lane can still try to reopen on the master.
        if !self.ssh_channels[..usable]
            .iter()
            .any(|channel| channel.has_negotiated())
            && !self.starting[..usable].contains(&true)
        {
            let waiting = self.waiting.iter().any(|item| !item.demand.ready());
            if attempt.speculative || !waiting {
                self.discard_prefetch();
                if let Some(channel) = self.ssh_channels[..usable]
                    .iter_mut()
                    .find(|channel| **channel == SshChannel::Unavailable)
                {
                    *channel = SshChannel::Spent;
                }
            } else {
                self.failure.get_or_insert_with(|| format!("{error:#}"));
            }
        }
        false
    }

    fn retry(&mut self, mut item: QueuedDemand, speculative: bool) {
        if !speculative || item.demand.required() {
            item.queued_at = Instant::now();
            self.waiting.push_back(item);
        } else {
            self.retire_flight(item.oid, &item.demand);
        }
    }

    fn retire_flight(&mut self, oid: Oid, demand: &Demand) {
        if self
            .pending
            .get(&oid)
            .is_some_and(|current| Arc::ptr_eq(&current.0, &demand.0))
        {
            self.pending.remove(&oid);
        }
    }

    fn cancel_queued(&mut self) {
        let queued = std::mem::take(&mut self.waiting)
            .into_iter()
            .chain(std::mem::take(&mut self.ahead));
        for item in queued {
            item.demand.complete(Batch::new(
                vec![item.oid],
                Err(object::OperationCancelled.into()),
            ));
        }
    }

    fn join_extra(&mut self, oid: Oid) -> Option<QueuedDemand> {
        for requests in [&mut self.waiting, &mut self.ahead] {
            if let Some(index) = requests.iter().position(|item| item.oid == oid) {
                return requests.remove(index);
            }
        }
        (!self.pending.contains_key(&oid)).then(|| self.insert(oid))
    }

    fn complete_extra(&mut self, oid: Oid) {
        if let Some(demand) = self.pending.remove(&oid) {
            demand.complete(Batch::new(vec![oid], Ok(Acquired::ValidatedExtra)));
        }
    }

    fn discard_prefetch(&mut self) {
        for item in self.ahead.drain(..) {
            self.pending.remove(&item.oid);
        }
    }
}

#[derive(Default)]
pub(crate) struct Metrics {
    pub(crate) oids: u64,
    pub(crate) packs: u64,
    pub(crate) bytes: u64,
    pub(crate) streamed_packs: u64,
    pub(crate) prefetches: u64,
    pub(crate) prefetched_blobs: u64,
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    work: Condvar,
    stop: Arc<AtomicBool>,
    finishing_used_packs: Arc<AtomicBool>,
    additional_starts: AtomicU64,
    http2_fetches: AtomicU64,
    http2_peak: AtomicU64,
    oids: AtomicU64,
    packs: AtomicU64,
    bytes: AtomicU64,
    streamed_packs: AtomicU64,
    prefetches: AtomicU64,
    prefetched_blobs: AtomicU64,
}

struct Http2Batch {
    work: Work,
    started: Instant,
}

enum NativeWork {
    Connect,
    Fetch {
        work: Work,
        failure: Option<String>,
        last_request: bool,
    },
    Snapshot(SnapshotRequest, Option<String>),
}

struct SnapshotRequest {
    oid: Oid,
    finished: mpsc::Sender<Result<Option<pack_stream::Received>>>,
}

pub(crate) struct Pool {
    shared: Arc<Shared>,
    scratch: RawBlobs,
    workers: Mutex<Vec<JoinHandle<Result<()>>>>,
    wake: Option<Arc<UnixStream>>,
    ssh_snapshot: Option<mpsc::Sender<SnapshotRequest>>,
}

/// Waiting after a successful command must not take the Snapshot lock: an
/// escaped reader may still hold it while waiting for unrelated network I/O.
#[derive(Clone)]
pub(crate) struct PackVerification {
    pool: Arc<Mutex<ObservedPool>>,
    scratch: RawBlobs,
    complete: Arc<AtomicBool>,
}

#[derive(Default)]
struct ObservedPool {
    shared: Weak<Shared>,
    mount_reads: MountReads,
}

impl PackVerification {
    pub(crate) fn new(scratch: &RawBlobs) -> Self {
        Self {
            pool: Arc::default(),
            scratch: scratch.clone(),
            complete: Arc::default(),
        }
    }

    pub(crate) fn observe(&self, pool: &Pool) {
        let mut observed = self.pool.lock().unwrap();
        pool.shared.queue.lock().unwrap().mount_reads = observed.mount_reads;
        observed.shared = Arc::downgrade(&pool.shared);
    }

    pub(crate) fn mounted(&self) {
        self.advance_mount_reads(MountReads::Lookup);
    }

    pub(crate) fn enumerating(&self) {
        self.advance_mount_reads(MountReads::Enumeration);
    }

    fn advance_mount_reads(&self, phase: MountReads) {
        let mut observed = self.pool.lock().unwrap();
        if phase > observed.mount_reads {
            observed.mount_reads = phase;
            if let Some(shared) = observed.shared.upgrade() {
                shared.queue.lock().unwrap().mount_reads = phase;
            }
        }
    }

    pub(crate) fn wait(&self) -> Result<()> {
        let Some(shared) = self.pool.lock().unwrap().shared.upgrade() else {
            self.complete.store(true, Ordering::Release);
            return Ok(());
        };
        shared.finishing_used_packs.store(true, Ordering::Release);
        let mut queue = shared.queue.lock().unwrap();
        shared.work.notify_all();
        loop {
            object::check_transfer()?;
            if let Some(error) = queue.recorded_failure() {
                bail!("remote object loading failed; execution incomplete: {error}");
            }
            if !queue
                .active
                .values()
                .any(|pack| pack.supplied_data(&self.scratch))
            {
                // Idle channels can confirm their exit while we unmount.
                // Busy channels can still have abandoned requests. Their
                // shutdown must not wait for unrelated responses.
                queue.ssh_exit_checks = Some(queue.busy.map(|busy| !busy));
                queue.cancel_queued();
                self.complete.store(true, Ordering::Release);
                shared.work.notify_all();
                return Ok(());
            }
            // Signals are handled independently of the transport's condition
            // variable. This wait observes them without setting a deadline.
            queue = shared
                .work
                .wait_timeout(queue, Duration::from_millis(50))
                .unwrap()
                .0;
        }
    }

    pub(crate) fn complete(&self) -> bool {
        self.complete.load(Ordering::Acquire)
    }
}

impl Pool {
    pub(crate) fn new(first: Session, connection: Connection, scratch: &RawBlobs) -> Result<Self> {
        let mut pool = Self::prepare_native(&connection, scratch, Vec::new())?;
        pool.attach_primary(first, connection)?;
        Ok(pool)
    }

    pub(crate) fn prepare_native(
        connection: &Connection,
        scratch: &RawBlobs,
        standby: Vec<StartingSession>,
    ) -> Result<Self> {
        let mut pool = Self {
            shared: Arc::default(),
            scratch: scratch.clone(),
            workers: Mutex::default(),
            wake: None,
            ssh_snapshot: None,
        };
        {
            let initial = 1 + standby.len().min(CONNECTIONS - 1);
            let mut queue = pool.shared.queue.lock().unwrap();
            queue.initial_channels = initial;
            queue.starting[1..initial].fill(true);
        }
        let mut standby = standby.into_iter();
        for index in 1..CONNECTIONS {
            pool.start(index, None, connection.clone(), standby.next(), None)?;
        }
        Ok(pool)
    }

    pub(crate) fn attach_primary(
        &mut self,
        mut first: Session,
        connection: Connection,
    ) -> Result<()> {
        first.set_stop(self.shared.stop.clone());
        let snapshots = connection.is_ssh().then(|| {
            self.shared.queue.lock().unwrap().ssh_channels[0] =
                SshChannel::Ready(first.process_id());
            let (send, receive) = mpsc::channel();
            self.ssh_snapshot = Some(send);
            receive
        });
        self.start(0, Some(first), connection, None, snapshots)
    }

    pub(crate) fn http2(
        configuration: crate::http2::Configuration,
        selection: crate::http2::Selection,
        scratch: &RawBlobs,
    ) -> Result<Option<(Self, Option<Oid>)>> {
        let (read, write) = UnixStream::pair()?;
        read.set_nonblocking(true)?;
        write.set_nonblocking(true)?;
        let signal = Arc::new(write);
        let worker_signal = signal.clone();
        let shared = Arc::<Shared>::default();
        let worker_state = shared.clone();
        let worker_scratch = scratch.clone();
        let (ready, result) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("git-on-http2".into())
            .spawn(move || {
                let client = crate::http2::Client::negotiate(
                    configuration,
                    worker_state.stop.clone(),
                    selection,
                    crate::transport::FetchMode::Filtered,
                );
                match client {
                    Ok(Some((client, selected))) => {
                        if ready.send(Ok(Some(selected))).is_ok() {
                            worker_state.run_http2(client, read, worker_signal, &worker_scratch);
                        }
                    }
                    Err(error) if error.is::<crate::http2::AuthenticationRejected>() => {
                        let _ = ready.send(Err(error));
                    }
                    _ => {
                        // Discovery sent no wants and reported no rejected credential.
                        let _ = ready.send(Ok(None));
                    }
                }
                Ok(())
            })?;
        let pool = Self {
            shared,
            scratch: scratch.clone(),
            workers: Mutex::new(vec![worker]),
            wake: Some(signal),
            ssh_snapshot: None,
        };
        let ready = result
            .recv()
            .context("Git HTTP/2 dispatcher stopped during discovery")??;
        object::check_transfer()?;
        Ok(ready.map(|selected| (pool, selected)))
    }

    fn wake(&self) {
        self.shared.work.notify_all();
        if let Some(wake) = self.wake.as_ref() {
            signal_http2(wake);
        }
    }

    fn start(
        &mut self,
        index: usize,
        session: Option<Session>,
        connection: Connection,
        standby: Option<StartingSession>,
        snapshots: Option<mpsc::Receiver<SnapshotRequest>>,
    ) -> Result<()> {
        let shared = self.shared.clone();
        let scratch = self.scratch.clone();
        let worker = thread::Builder::new()
            .name(format!("git-on-native-{index}"))
            .spawn(move || {
                shared.worker(index, session, connection, &scratch, standby, snapshots)
            })?;
        self.workers.get_mut().unwrap().push(worker);
        Ok(())
    }

    pub(crate) fn request(&self, oid: Oid) -> Result<Demand> {
        self.request_with_batching(oid, false)
    }

    pub(crate) fn request_with_batching(&self, oid: Oid, immediate: bool) -> Result<Demand> {
        self.request_with_lookup(oid, immediate, || self.scratch.lookup_demand(oid))
    }

    pub(crate) fn fetch_ssh_snapshot(&self, oid: Oid) -> Result<Option<pack_stream::Received>> {
        let Some(send) = &self.ssh_snapshot else {
            return Ok(None);
        };
        object::check_transfer()?;
        let (finished, result) = mpsc::channel();
        {
            // Enqueue while the primary tests its queue, so it cannot miss the
            // wakeup when the mount has no more reads to schedule.
            let _queue = self.shared.queue.lock().unwrap();
            send.send(SnapshotRequest { oid, finished })
                .map_err(|_| anyhow::anyhow!("SSH snapshot channel stopped"))?;
        }
        self.wake();
        result.recv().context("SSH snapshot worker stopped")?
    }

    fn request_with_lookup(
        &self,
        oid: Oid,
        immediate: bool,
        lookup: impl FnOnce() -> DemandLookup,
    ) -> Result<Demand> {
        object::check_transfer()?;
        let mut queue = self.shared.queue.lock().unwrap();
        if self.shared.stop.load(Ordering::Relaxed) || queue.ssh_exit_checks.is_some() {
            return Err(object::OperationCancelled.into());
        }
        if let Some(error) = &queue.failure {
            bail!("remote object transport previously failed: {error}");
        }
        // Publication can race a caller's first cache lookup. See the pending
        // flight or the stored object under the same lock that retires flights.
        let pending = queue
            .pending
            .contains_key(&oid)
            .then(|| queue.foreground(oid));
        let lookup = lookup();
        if lookup == DemandLookup::Missing {
            let demand = pending.unwrap_or_else(|| queue.foreground(oid));
            if immediate {
                if let Some(item) = queue.waiting.iter_mut().find(|item| item.oid == oid) {
                    item.immediate = true;
                }
            }
            self.wake();
            return Ok(demand);
        }
        let joins_flight = pending.is_some();
        let demand = pending.unwrap_or_default();
        if joins_flight && lookup == DemandLookup::ClaimedForecast {
            self.shared
                .complete_object(oid, &demand, true, Acquired::Stored);
        } else {
            demand.complete(Batch::new(vec![oid], Ok(Acquired::Stored)));
        }
        Ok(demand)
    }

    pub(crate) fn prefetch<I: Iterator<Item = Oid>>(
        &self,
        anchor: Oid,
        candidates: impl FnOnce(bool) -> I,
        mut in_git: impl FnMut(Oid) -> bool,
    ) {
        let mut queue = self.shared.queue.lock().unwrap();
        if self.shared.stop.load(Ordering::Relaxed)
            || queue.ssh_exit_checks.is_some()
            || queue.failure.is_some()
            || queue.native_ceiling == Some(1)
        {
            return;
        }
        // Only the first real access to a predicted OID earns credit.
        // Keep a bounded distance ahead of those observed accesses.
        let width = queue.forecast_width(anchor);
        let limit = queue.prediction_room(width);
        if limit == 0 {
            return;
        }
        let mut accepted = 0;
        let prioritize_opened = !matches!(queue.mode, ConnectionMode::Standard) && queue.dense();
        for oid in candidates(prioritize_opened) {
            if queue.pending.contains_key(&oid)
                || queue.predicted.contains_key(&oid)
                || self.scratch.available(oid)
                || in_git(oid)
            {
                continue;
            }
            let step = queue.forecast_step;
            queue.predicted.insert(oid, step);
            let mut entry = queue.insert(oid);
            entry.prefetch_width = width;
            queue.ahead.push_back(entry);
            accepted += 1;
            if accepted == limit {
                break;
            }
        }
        if accepted != 0 {
            self.wake();
        }
    }

    pub(crate) fn retire(&self, oids: &[Oid]) {
        let mut queue = self.shared.queue.lock().unwrap();
        for oid in oids {
            queue.pending.remove(oid);
        }
    }

    pub(crate) fn failure(&self) -> Option<String> {
        self.shared
            .queue
            .lock()
            .unwrap()
            .recorded_failure()
            .map(str::to_owned)
    }

    pub(crate) fn additional_starts(&self) -> u64 {
        self.shared.additional_starts.load(Ordering::Relaxed)
    }

    pub(crate) fn metrics(&self) -> Metrics {
        let s = &self.shared;
        Metrics {
            oids: s.oids.load(Ordering::Relaxed),
            packs: s.packs.load(Ordering::Relaxed),
            bytes: s.bytes.load(Ordering::Relaxed),
            streamed_packs: s.streamed_packs.load(Ordering::Relaxed),
            prefetches: s.prefetches.load(Ordering::Relaxed),
            prefetched_blobs: s.prefetched_blobs.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn http2_metrics(&self) -> (u64, u64) {
        (
            self.shared.http2_fetches.load(Ordering::Relaxed),
            self.shared.http2_peak.load(Ordering::Relaxed),
        )
    }

    pub(crate) fn stop(&self) -> Result<()> {
        {
            let mut queue = self.shared.queue.lock().unwrap();
            self.shared.stop.store(true, Ordering::Relaxed);
            queue.cancel_queued();
        }
        self.wake();
        let mut outcome = Ok(());
        for worker in self.workers.lock().unwrap().drain(..) {
            let result = worker
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Git transport worker panicked")));
            if outcome.is_ok() {
                outcome = result;
            }
        }
        outcome
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

impl Shared {
    fn complete_object(&self, oid: Oid, demand: &Demand, speculative: bool, acquired: Acquired) {
        demand.complete_with(Batch::new(vec![oid], Ok(acquired)), |state| {
            if !state.counted {
                state.counted = true;
                self.oids.fetch_add(1, Ordering::Relaxed);
                if speculative {
                    self.prefetched_blobs.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
    }

    fn response(
        self: &Arc<Self>,
        work: &Work,
        oids: &[Oid],
        scratch: &RawBlobs,
    ) -> pack_stream::Response<'static> {
        let active = work.active_pack();
        self.queue
            .lock()
            .unwrap()
            .active
            .insert(work.id(), active.clone());
        let shared = self.clone();
        let mut cache = scratch.clone();
        let transfer = work.transfer.clone();
        let speculative = work.speculative;
        let tasks = work.readers.clone();
        let mut response = pack_stream::Response::new(
            scratch.clone(),
            oids.to_vec(),
            Box::new(move |object| {
                let oid = object.oid();
                let demand = &tasks
                    .iter()
                    .find(|(wanted, _)| *wanted == oid)
                    .expect("known want")
                    .1;
                let publish = |acquired| {
                    shared.complete_object(oid, demand, speculative, acquired);
                };
                if object.kind() == git2::ObjectType::Blob {
                    cache.insert_verified_blob(
                        object,
                        Some(transfer.clone()),
                        speculative,
                        || !speculative || demand.required(),
                        |already_stored| {
                            // An existing blob can have been exposed by a
                            // verified extra whose flight completion still
                            // holds the queue. Let that completion win before
                            // reporting the duplicate's successful decode.
                            let _publication = already_stored.then(|| shared.queue.lock().unwrap());
                            publish(Acquired::Stored);
                        },
                    )
                } else {
                    anyhow::ensure!(!speculative, "prefetched object is not a blob");
                    // A validated extra can supply a tree before this
                    // duplicate response finishes decoding it.
                    let _publication = shared.queue.lock().unwrap();
                    publish(Acquired::Object(object));
                    Ok(())
                }
            }),
        );
        let shared = self.clone();
        let extras = work.extras.clone();
        response.on_pending_extra(move |oid| shared.join_pack_extra(&extras, oid));
        let extras = work.extras.clone();
        let scratch = scratch.clone();
        let shared = self.clone();
        response.set_decode_priority(move |remaining| {
            use crate::raw_blob::PackDecodePriority;

            if shared.finishing_used_packs.load(Ordering::Acquire) && active.supplied_data(&scratch)
            {
                PackDecodePriority::Completion
            } else if !speculative
                || active.readers.iter().any(|(oid, demand)| {
                    demand.required() && remaining.contains(oid) && !demand.ready()
                })
                || extras.lock().unwrap().as_deref().is_some_and(|extras| {
                    extras
                        .iter()
                        .any(|item| item.demand.required() && !item.demand.ready())
                })
            {
                PackDecodePriority::Demand
            } else {
                PackDecodePriority::Speculative
            }
        });
        response
    }

    fn join_pack_extra(&self, extras: &Mutex<Option<Vec<QueuedDemand>>>, oid: Oid) {
        let mut queue = self.queue.lock().unwrap();
        if queue.ssh_exit_checks.is_some() {
            return;
        }
        let mut extras = extras.lock().unwrap();
        let Some(extras) = extras.as_mut() else {
            return;
        };
        if let Some(item) = queue.join_extra(oid) {
            extras.push(item);
        }
    }

    fn finish(
        &self,
        mut work: Work,
        result: Result<pack_stream::Received>,
        started: Instant,
        lane: Option<usize>,
        scratch: &RawBlobs,
    ) {
        // Treat a validated extra and the completion of its existing flight
        // as one publication with respect to requests and failure reporting.
        let mut queue = self.queue.lock().unwrap();
        queue.active.remove(&work.id());
        let mut validated = Vec::new();
        let result = result.map(|mut pack| {
            work.transfer.mark_verified();
            pack.cache_extras(scratch, Some(&work.transfer), |oid| validated.push(oid));
            self.packs.fetch_add(1, Ordering::Relaxed);
            self.bytes.fetch_add(pack.bytes, Ordering::Relaxed);
            if lane.is_none() {
                self.http2_fetches.fetch_add(1, Ordering::Relaxed);
            }
            if work.speculative {
                self.prefetches.fetch_add(1, Ordering::Relaxed);
            }
            self.streamed_packs.fetch_add(1, Ordering::Relaxed);
        });

        if let Some(index) = lane {
            queue.busy[index] = false;
        }
        let required = result.is_err()
            && Self::required_after_failure(
                &work.readers,
                scratch,
                &work.transfer,
                work.speculative,
            );
        // libcurl can finish a failed transfer while its decoder processes the
        // last packet. Close registration under the queue lock before leaving
        // any waiters for another request.
        let extras = work.extras.lock().unwrap().take().expect("pack is active");
        if result.is_ok() {
            // A different pack can validate the base while the pack that
            // reserved its flight is still waiting for its trailer.
            for oid in validated {
                queue.complete_extra(oid);
            }
        } else {
            work.items
                .extend(extras.into_iter().filter(|item| !item.demand.ready()));
        }
        // Retire a speculative success from the queue once its verified blob
        // is visible in the raw store. A foreground waiter still holds a clone.
        work.items.retain(|item| {
            if !item.demand.ready() {
                return true;
            }
            if work.speculative && !item.demand.required() {
                queue.retire_flight(item.oid, &item.demand);
            }
            false
        });
        let result = match result {
            Ok(()) => {
                queue.mode.record_reply(started.elapsed());
                None
            }
            Err(error)
                if !required
                    && !self.stop.load(Ordering::Relaxed)
                    && queue.ssh_exit_checks.is_none()
                    && queue.failure.is_none()
                    && !object::is_cancelled(&error) =>
            {
                for item in work.items {
                    queue.retry(item, true);
                }
                self.work.notify_all();
                return;
            }
            Err(error) => {
                if required && !object::is_cancelled(&error) {
                    queue.failure.get_or_insert_with(|| format!("{error:#}"));
                }
                Some(Err(error))
            }
        };
        drop(queue);
        if let Some(result) = result {
            let batch = Batch::new(work.wants(), result);
            for item in work.items {
                item.demand.complete(batch.clone());
            }
        }
        self.work.notify_all();
    }

    fn report_http2_failure(
        self: &Arc<Self>,
        work: &Work,
        cache: &RawBlobs,
    ) -> crate::http2::ReportFailure {
        let readers = work.readers.clone();
        let transfer = work.transfer.clone();
        let speculative = work.speculative;
        let shared = self.clone();
        let cache = cache.clone();
        Arc::new(move |error| {
            let mut queue = shared.queue.lock().unwrap();
            if Self::required_after_failure(&readers, &cache, &transfer, speculative) {
                queue.failure.get_or_insert_with(|| format!("{error:#}"));
            } else if speculative {
                // The decoder can still be retiring this failed response.
                // A new reader must not join a completed forecast whose
                // bytes the failure just invalidated.
                for (oid, demand) in readers.iter() {
                    if !demand.required() {
                        queue.retire_flight(*oid, demand);
                    }
                }
            }
        })
    }

    fn required_after_failure(
        readers: &[(Oid, Demand)],
        cache: &RawBlobs,
        transfer: &Arc<Transfer>,
        speculative: bool,
    ) -> bool {
        if speculative {
            cache.fail_forecast(
                transfer,
                readers
                    .iter()
                    .map(|(oid, demand)| (*oid, demand.delivered())),
            )
        } else {
            readers
                .iter()
                .any(|(oid, demand)| demand.needs_exact_pack(*oid, cache, transfer))
        }
    }

    fn run_http2(
        self: &Arc<Self>,
        mut client: crate::http2::Client,
        mut wake: UnixStream,
        signal: Arc<UnixStream>,
        scratch: &RawBlobs,
    ) {
        let outcome = self.drive_http2(&mut client, &mut wake, &signal, scratch);
        self.finish_http2(outcome.err().filter(|error| !object::is_cancelled(error)));
    }

    fn finish_http2(&self, failure: Option<anyhow::Error>) {
        let pending = {
            let mut queue = self.queue.lock().unwrap();
            if let Some(error) = &failure {
                queue.failure.get_or_insert_with(|| format!("{error:#}"));
            }
            queue.waiting.clear();
            queue.ahead.clear();
            std::mem::take(&mut queue.pending)
        };
        for (oid, demand) in pending {
            let error = failure.as_ref().map_or_else(
                || object::OperationCancelled.into(),
                |error| anyhow::anyhow!("Git HTTP/2 dispatcher failed: {error:#}"),
            );
            demand.complete(Batch::new(vec![oid], Err(error)));
        }
        self.work.notify_all();
    }

    fn drive_http2(
        self: &Arc<Self>,
        client: &mut crate::http2::Client,
        wake: &mut UnixStream,
        decoder_wake: &Arc<UnixStream>,
        scratch: &RawBlobs,
    ) -> Result<()> {
        let mut active = HashMap::<usize, Http2Batch>::new();
        let mut token = 0_usize;
        while !self.stop.load(Ordering::Relaxed) {
            object::check_transfer()?;
            drain_wake(wake)?;
            let (waiting, ahead, failure, delay) = {
                let mut queue = self.queue.lock().unwrap();
                let delay = queue
                    .failure
                    .is_none()
                    .then(|| queue.http2_batch_wait(Instant::now(), !active.is_empty()))
                    .flatten();
                let (waiting, ahead) = if delay.is_none() {
                    (
                        std::mem::take(&mut queue.waiting),
                        std::mem::take(&mut queue.ahead),
                    )
                } else {
                    (VecDeque::new(), VecDeque::new())
                };
                (waiting, ahead, queue.failure.clone(), delay)
            };
            for (mut batches, speculative) in [(waiting, false), (ahead, true)] {
                while let Some(work) = Work::take(&mut batches, speculative) {
                    let started = Instant::now();
                    let oids = work.wants();
                    let submitted = match &failure {
                        Some(error) => Err(anyhow::anyhow!("{error}")),
                        None => {
                            token = token
                                .checked_add(1)
                                .context("Git HTTP/2 request ID overflow")?;
                            client.start(
                                token,
                                &oids,
                                self.response(&work, &oids, scratch),
                                decoder_wake,
                                Some(self.report_http2_failure(&work, scratch)),
                            )
                        }
                    };
                    match submitted {
                        Ok(()) => {
                            active.insert(token, Http2Batch { work, started });
                        }
                        Err(error) => self.finish(work, Err(error), started, None, scratch),
                    }
                    self.http2_peak
                        .fetch_max(active.len() as u64, Ordering::Relaxed);
                }
            }
            for (token, pack) in client.progress()? {
                let batch = active
                    .remove(&token)
                    .context("unknown completed Git demand")?;
                self.finish(batch.work, pack, batch.started, None, scratch);
            }
            let timeout = delay.map_or(Duration::from_secs(1), |remaining| {
                remaining.max(Duration::from_millis(1))
            });
            // A failed prefetch can enqueue an exact retry from this thread.
            let wait = self
                .queue
                .lock()
                .unwrap()
                .wait_for_http2_event(delay, !active.is_empty());
            if wait {
                client.wait(wake, timeout)?;
            }
        }
        Ok(())
    }

    fn worker(
        self: &Arc<Self>,
        index: usize,
        mut session: Option<Session>,
        connection: Connection,
        scratch: &RawBlobs,
        standby: Option<StartingSession>,
        snapshots: Option<mpsc::Receiver<SnapshotRequest>>,
    ) -> Result<()> {
        let ssh = connection.is_ssh();
        let mut used = session.as_ref().is_some_and(Session::completed_fetch);
        let mut completed = Vec::<ActivePack>::new();
        let mut sent_shutdown_eof = false;
        if let Some(mut standby) = standby {
            standby.set_stop(self.stop.clone());
            session = standby.negotiate().ok().flatten();
            let mut queue = self.queue.lock().unwrap();
            if let Some(session) = &session {
                queue.ssh_channels[index] = SshChannel::Ready(session.process_id());
                self.additional_starts.fetch_add(1, Ordering::Relaxed);
            }
            queue.finished_ssh_standby(index);
            self.work.notify_all();
        }
        if let Some(session) = session.as_mut().filter(|_| ssh) {
            session.close_input_after_command(self.finishing_used_packs.clone());
        }
        loop {
            let next = {
                let mut queue = self.queue.lock().unwrap();
                let needs_ssh_channel = session.is_none() && ssh;
                loop {
                    let confirm_exit =
                        ssh && queue.ssh_exit_checks.is_some_and(|checks| checks[index]);
                    if self.stop.load(Ordering::Relaxed)
                        || queue.native_ceiling.is_some_and(|ceiling| index >= ceiling)
                        || confirm_exit
                    {
                        drop(queue);
                        if let Some(session) = session {
                            if confirm_exit
                                && (used
                                    || completed.iter().any(|pack| pack.supplied_data(scratch)))
                            {
                                if let Err(error) = session.confirm_ssh_exit() {
                                    self.queue
                                        .lock()
                                        .unwrap()
                                        .failure
                                        .get_or_insert_with(|| format!("{error:#}"));
                                }
                            }
                            return session.stop();
                        }
                        return Ok(());
                    }
                    if ssh && self.finishing_used_packs.load(Ordering::Acquire) {
                        if let Some(session) = session.as_ref().filter(|_| !sent_shutdown_eof) {
                            // Give idle peers a head start while other packs finish.
                            // The final exit check still owns error reporting.
                            let _ = session.close_input();
                            sent_shutdown_eof = true;
                        }
                        queue = self.work.wait(queue).unwrap();
                        continue;
                    }
                    if queue.ssh_exit_checks.is_some() {
                        queue = self.work.wait(queue).unwrap();
                        continue;
                    }
                    if let Some(request) = snapshots.as_ref().and_then(|r| r.try_recv().ok()) {
                        break NativeWork::Snapshot(request, queue.failure.clone());
                    }
                    if let Some(delay) =
                        queue.batch_wait(Instant::now(), queue.busy.contains(&true))
                    {
                        queue = self.work.wait_timeout(queue, delay).unwrap().0;
                        continue;
                    }
                    if let Some(work) = queue.native_work(index, needs_ssh_channel) {
                        break work;
                    }
                    queue = self.work.wait(queue).unwrap();
                }
            };
            self.work.notify_all();
            let (work, failure, last_request) = match next {
                NativeWork::Fetch {
                    work,
                    failure,
                    last_request,
                } => (work, failure, last_request),
                NativeWork::Snapshot(request, failure) => {
                    let received = (|| {
                        if let Some(error) = failure {
                            bail!("remote object transport previously failed: {error}");
                        }
                        let Some(session) = session.take() else {
                            return Ok(None);
                        };
                        connection.check()?;
                        let response = pack_stream::Response::for_materialization(
                            scratch.clone(),
                            request.oid,
                        );
                        session.reuse_ssh_snapshot(request.oid, response)
                    })();
                    let _ = request.finished.send(received);
                    return Ok(());
                }
                NativeWork::Connect => {
                    let connected =
                        connection
                            .connect_additional(self.stop.clone())
                            .and_then(|session| {
                                session.context("native Git cannot open a reusable demand channel")
                            });
                    let mut queue = self.queue.lock().unwrap();
                    queue.busy[index] = false;
                    match connected {
                        Ok(mut connected) => {
                            queue.opening_ssh = None;
                            queue.ssh_channels[index] = SshChannel::Ready(connected.process_id());
                            connected.close_input_after_command(self.finishing_used_packs.clone());
                            session = Some(connected);
                            self.additional_starts.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(error) if !self.stop.load(Ordering::Relaxed) => {
                            if queue.rejected_ssh_channel(index, error) {
                                self.stop.store(true, Ordering::Relaxed);
                            }
                        }
                        Err(_) => queue.opening_ssh = None,
                    }
                    self.work.notify_all();
                    continue;
                }
            };
            let started = Instant::now();
            if ssh && failure.is_none() && session.as_ref().is_some_and(Session::has_exited) {
                // No request was sent on this channel. Return the work to the
                // queue and preserve the status of any earlier responses.
                let expired = session.take().unwrap();
                let result = expired.confirm_ssh_exit().and(expired.stop());
                let mut queue = self.queue.lock().unwrap();
                if let Err(error) = result {
                    queue.record_channel_failure(used, &mut completed, &error, scratch);
                }
                used = false;
                completed.clear();
                queue.ssh_channels[index] = SshChannel::Spent;
                queue.busy[index] = false;
                for item in work.items {
                    queue.retry(item, work.speculative);
                }
                if queue.ssh_exit_checks.is_some() || self.stop.load(Ordering::Relaxed) {
                    queue.cancel_queued();
                }
                self.work.notify_all();
                continue;
            }
            if !ssh && session.is_none() && failure.is_none() {
                let connected =
                    connection
                        .connect_additional(self.stop.clone())
                        .and_then(|session| {
                            session.context("native Git cannot open a reusable demand channel")
                        });
                match connected {
                    Ok(new) => {
                        if index != 0 {
                            self.additional_starts.fetch_add(1, Ordering::Relaxed);
                        }
                        session = Some(new);
                    }
                    Err(error) => {
                        self.finish(work, Err(error), started, Some(index), scratch);
                        continue;
                    }
                }
            }
            let result = if let Some(error) = failure {
                Err(anyhow::anyhow!("{error}"))
            } else {
                let oids = work.wants();
                connection.check().and_then(|_| {
                    let response = self.response(&work, &oids, scratch);
                    if last_request {
                        session.take().unwrap().fetch_last_ssh(&oids, response)
                    } else {
                        session.as_mut().unwrap().fetch_stream(
                            &oids,
                            crate::transport::FetchMode::Filtered,
                            response,
                        )
                    }
                })
            };
            if let Err(error) = &result {
                let mut queue = self.queue.lock().unwrap();
                if ssh && queue.ssh_channels[index].is_ready() {
                    queue.ssh_channels[index] = SshChannel::Spent;
                }
                if !connection.is_stateless()
                    && (error.is::<crate::transport::FailedTransportExit>()
                        || error.is::<crate::transport::FailedTransportCompletion>()
                            && !object::is_cancelled(error))
                {
                    queue.record_channel_failure(used, &mut completed, error, scratch);
                }
                drop(queue);
                // fetch_stream reaps a channel with an incomplete response.
                session = None;
                used = false;
                completed.clear();
            } else if last_request {
                used = false;
                completed.clear();
            } else if !connection.is_stateless() && !used {
                let pack = work.active_pack();
                if pack.supplied_data(scratch) {
                    used = true;
                    completed.clear();
                } else {
                    completed.push(pack);
                }
            }
            self.finish(work, result, started, Some(index), scratch);
        }
    }
}

pub(crate) fn signal_http2(mut wake: &UnixStream) {
    // A full signal socket is already readable.
    let _ = wake.write(&[1]);
}

fn drain_wake(wake: &mut UnixStream) -> io::Result<()> {
    let mut buffer = [0; 256];
    loop {
        match wake.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(_) => (),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => (),
            Err(error) => return Err(error),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_late_pool_inherits_the_current_mount_phase() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = RawBlobs::new(directory.path()).unwrap();
        let verification = PackVerification::new(&scratch);
        verification.mounted();
        for phase in [MountReads::Lookup, MountReads::Enumeration] {
            let pool = Pool {
                shared: Arc::default(),
                scratch: scratch.clone(),
                workers: Mutex::default(),
                wake: None,
                ssh_snapshot: None,
            };
            verification.observe(&pool);
            assert!(pool.shared.queue.lock().unwrap().mount_reads == phase);
            verification.enumerating();
            verification.mounted();
        }
    }

    #[test]
    fn the_first_validated_pack_completes_an_existing_base_flight() {
        let mut queue = Queue::default();
        let oid = Oid::from_bytes(&[23; 20]).unwrap();
        let first = queue.join_extra(oid).unwrap();
        let reader = queue.foreground(oid);
        assert!(!reader.ready());
        assert!(queue.waiting.is_empty());
        assert!(queue.join_extra(oid).is_none());
        queue.complete_extra(oid);
        assert!(reader.ready());
        assert!(first.demand.ready());
        assert!(!queue.pending.contains_key(&oid));
    }

    #[test]
    fn startup_exact_demand_uses_a_prepared_channel() {
        let oid = Oid::from_bytes(&[7; 20]).unwrap();
        let mut queue = Queue {
            initial_channels: 5,
            ..Queue::default()
        };
        queue.busy[..3].fill(true);
        queue.starting[3] = true;
        queue.foreground(oid);
        assert!(queue.native_work(4, true).is_none());
        let Some(NativeWork::Fetch {
            work,
            failure: None,
            ..
        }) = queue.native_work(4, false)
        else {
            panic!("the prepared SSH channel must serve the exact request");
        };
        assert_eq!(work.wants(), [oid]);
    }

    #[test]
    fn expand_only_when_every_startup_reply_is_slow() {
        let mut variable = ConnectionMode::default();
        let mut slow = ConnectionMode::default();
        for _ in 1..REPLY_SAMPLES {
            slow.record_reply(SLOW_REPLY);
            variable.record_reply(SLOW_REPLY);
        }
        variable.record_reply(SLOW_REPLY / 2);
        for _ in 0..REPLY_SAMPLES {
            variable.record_reply(SLOW_REPLY);
        }
        assert!(matches!(variable, ConnectionMode::Standard));
        slow.record_reply(SLOW_REPLY);
        slow.record_reply(SLOW_REPLY / 2);
        assert!(matches!(slow, ConnectionMode::Wide));
    }

    #[test]
    fn speculative_pack_stays_at_four_wants_when_forecasts_overlap() {
        let mut queue = Queue::default();
        for index in 0..6 {
            let item = queue.insert(Oid::from_bytes(&[index; 20]).unwrap());
            queue.ahead.push_back(item);
        }
        let first = Work::take(&mut queue.ahead, true).unwrap();
        assert_eq!(first.items.len(), 4);
        let next = Work::take(&mut queue.ahead, true).unwrap();
        assert_eq!(next.items.len(), 2);
    }

    #[test]
    fn forecast_grows_with_distinct_hits_and_shrinks_on_misses() {
        for mode in [ConnectionMode::Probing(0), ConnectionMode::Standard] {
            let mut queue = Queue {
                mode,
                ..Queue::default()
            };
            for index in 0..6 {
                let oid = Oid::from_bytes(&[index; 20]).unwrap();
                queue.predicted.insert(oid, queue.forecast_step);
                let expected = match index {
                    0 => 4,
                    1..=2 => 64,
                    3..=4 => 128,
                    _ => 256,
                };
                assert_eq!(queue.forecast_width(oid), expected);
            }
            let mut previous = MAX_READ_AHEAD;
            for width in [128, 128, 64, 64, 4] {
                for _ in 1..FORECAST_MISSES {
                    assert_eq!(queue.forecast_width(Oid::zero()), previous);
                }
                assert_eq!(queue.forecast_width(Oid::zero()), width);
                previous = width;
            }
        }
    }

    #[test]
    fn fast_scan_batches_files_after_its_anchor() {
        let directory = tempfile::tempdir().unwrap();
        let anchor = Oid::from_bytes(&[1; 20]).unwrap();
        let earlier = Oid::from_bytes(&[2; 20]).unwrap();
        let following: [Oid; 5] =
            std::array::from_fn(|index| Oid::from_bytes(&[index as u8 + 3; 20]).unwrap());
        let mut queue = Queue {
            mode: ConnectionMode::Standard,
            forecast_score: DENSE_SCORE - 1,
            ..Queue::default()
        };
        queue.predicted.insert(anchor, 0);
        let pool = Pool {
            shared: Arc::new(Shared {
                queue: Mutex::new(queue),
                ..Shared::default()
            }),
            scratch: RawBlobs::new(directory.path()).unwrap(),
            workers: Mutex::default(),
            wake: None,
            ssh_snapshot: None,
        };
        pool.prefetch(
            anchor,
            |prioritize_opened| {
                following
                    .into_iter()
                    .map(move |oid| if prioritize_opened { earlier } else { oid })
            },
            |_| false,
        );
        let wants = {
            let mut queue = pool.shared.queue.lock().unwrap();
            Work::take(&mut queue.ahead, true).unwrap().wants()
        };
        assert_eq!(wants, following);
    }

    #[test]
    fn a_slow_overlapping_scan_can_expand_after_one_hit() {
        let anchor = Oid::from_bytes(&[1; 20]).unwrap();
        let other = Oid::from_bytes(&[2; 20]).unwrap();
        for (mode, complete_other, expected) in [
            (ConnectionMode::Probing(0), false, READ_AHEAD),
            (ConnectionMode::Standard, false, READ_AHEAD),
            (ConnectionMode::Probing(1), true, READ_AHEAD),
            (ConnectionMode::Probing(1), false, 64),
        ] {
            let mut queue = Queue {
                mode,
                ..Queue::default()
            };
            queue.foreground(anchor);
            let pending = queue.foreground(other);
            if complete_other {
                pending.complete(Batch::new(vec![other], Ok(Acquired::Stored)));
            }
            // Parallel requests alone leave the initial DFS pack at four.
            assert_eq!(queue.forecast_width(other), READ_AHEAD);
            queue.predicted.insert(anchor, queue.forecast_step);
            assert_eq!(queue.forecast_width(anchor), expected);
        }
    }

    #[test]
    fn old_completed_predictions_release_slots_but_live_requests_keep_them() {
        let mut queue = Queue {
            forecast_step: 12,
            ..Queue::default()
        };
        for index in 0..4 * READ_AHEAD as u8 {
            let oid = Oid::from_bytes(&[index; 20]).unwrap();
            queue.predicted.insert(oid, 4);
        }
        let active = Oid::from_bytes(&[1; 20]).unwrap();
        let recent = Oid::from_bytes(&[2; 20]).unwrap();
        queue.pending.insert(active, Demand::default());
        queue.predicted.insert(recent, 11);
        assert_eq!(queue.prediction_room(READ_AHEAD), READ_AHEAD);
        assert_eq!(queue.predicted.len(), 2);
        assert!(queue.predicted.contains_key(&active));
        assert!(queue.predicted.contains_key(&recent));
    }

    #[test]
    fn fast_forecasts_can_restart_after_a_widened_scan_misses() {
        let mut queue = Queue {
            mode: ConnectionMode::Standard,
            forecast_score: DENSE_SCORE - 1,
            ..Queue::default()
        };
        let anchor = Oid::from_bytes(&[1; 20]).unwrap();
        queue.predicted.insert(anchor, queue.forecast_step);
        assert_eq!(queue.forecast_width(anchor), 64);
        for value in 2..=24 {
            let oid = Oid::from_bytes(&[value; 20]).unwrap();
            queue.predicted.insert(oid, queue.forecast_step);
        }
        let active = Oid::from_bytes(&[2; 20]).unwrap();
        queue.pending.insert(active, Demand::default());
        for index in 0..=MIN_PREDICTION_AGE {
            queue.forecast_width(Oid::from_bytes(&[100 + index as u8; 20]).unwrap());
        }
        assert!(!queue.dense());
        assert_eq!(queue.prediction_room(READ_AHEAD), READ_AHEAD);
        assert_eq!(queue.predicted.len(), 1);
        assert!(queue.predicted.contains_key(&active));
    }

    #[test]
    fn failed_speculation_requeues_the_live_waiter() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = RawBlobs::new(directory.path()).unwrap();
        let shared = Shared::default();
        let used = Oid::from_bytes(&[1; 20]).unwrap();
        let unused = Oid::from_bytes(&[2; 20]).unwrap();
        let (work, demand) = {
            let mut queue = shared.queue.lock().unwrap();
            for oid in [used, unused] {
                let item = queue.insert(oid);
                queue.ahead.push_back(item);
            }
            let work = Work::take(&mut queue.ahead, true).unwrap();
            // The file became required after the speculative pack was sent.
            (work, queue.foreground(used))
        };
        shared.finish(
            work,
            Err(anyhow::anyhow!("prefetch refused")),
            Instant::now(),
            None,
            &scratch,
        );
        let queue = shared.queue.lock().unwrap();
        assert!(queue.failure.is_none());
        assert!(!queue.pending.contains_key(&unused));
        assert_eq!(queue.waiting.len(), 1);
        assert_eq!(queue.waiting[0].oid, used);
        assert!(Arc::ptr_eq(&demand.0, &queue.waiting[0].demand.0));
        assert!(!demand.ready());
    }

    #[test]
    fn retired_prefetch_cannot_claim_an_ordinary_read() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = RawBlobs::new(directory.path()).unwrap();
        let shared = Shared::default();
        let forecast = Oid::from_bytes(&[1; 20]).unwrap();
        let extra = Oid::from_bytes(&[2; 20]).unwrap();
        let (work, demand) = {
            let mut queue = shared.queue.lock().unwrap();
            let item = queue.insert(forecast);
            queue.ahead.push_back(item);
            (
                Work::take(&mut queue.ahead, true).unwrap(),
                queue.foreground(extra),
            )
        };
        let extras = work.extras.clone();
        shared.finish(
            work,
            Err(anyhow::anyhow!("HTTP/2 request ended")),
            Instant::now(),
            None,
            &scratch,
        );
        // A decoder already processing an entry may discover its OID after
        // curl reports the failure and the request has been retired.
        shared.join_pack_extra(&extras, extra);

        let queue = shared.queue.lock().unwrap();
        assert!(queue.failure.is_none());
        assert_eq!(queue.waiting.len(), 1);
        assert_eq!(queue.waiting[0].oid, extra);
        assert!(Arc::ptr_eq(&queue.waiting[0].demand.0, &demand.0));
    }

    #[test]
    fn failed_prefetch_ignores_reads_supplied_by_another_pack() {
        let directory = tempfile::tempdir().unwrap();
        let mut scratch = RawBlobs::new(directory.path()).unwrap();
        let shared = Arc::<Shared>::default();
        let data = b"blob from another pack";
        let oid = Oid::hash_object(git2::ObjectType::Blob, data).unwrap();
        let work = {
            let mut queue = shared.queue.lock().unwrap();
            let item = queue.insert(oid);
            queue.ahead.push_back(item);
            Work::take(&mut queue.ahead, true).unwrap()
        };
        scratch
            .insert_fetched(oid, data.len(), &data[..], None)
            .unwrap();
        assert!(scratch.for_read(oid).unwrap().is_some());
        let error = anyhow::anyhow!("unused forecast transfer failed");
        shared.report_http2_failure(&work, &scratch)(&error);
        assert!(shared.queue.lock().unwrap().failure.is_none());
        shared.finish(work, Err(error), Instant::now(), None, &scratch);
        assert!(shared.queue.lock().unwrap().failure.is_none());
    }

    fn pack_reply(entries: &[(git2::ObjectType, &[u8])], complete: bool) -> Vec<u8> {
        use sha1_checked::Digest;

        let mut pack = b"PACK\0\0\0\x02".to_vec();
        pack.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for (kind, data) in entries {
            let mut size = data.len();
            let mut header = (*kind as u8) << 4 | (size & 15) as u8;
            size >>= 4;
            while size != 0 {
                pack.push(header | 0x80);
                header = (size & 0x7f) as u8;
                size >>= 7;
            }
            pack.push(header);
            let mut encoder =
                flate2::write::ZlibEncoder::new(&mut pack, flate2::Compression::fast());
            encoder.write_all(data).unwrap();
            encoder.finish().unwrap();
        }
        if complete {
            let checksum = sha1_checked::Sha1::digest(&pack);
            pack.extend_from_slice(&checksum);
        }
        let mut reply = format!("000dpackfile\n{:04x}\x01", pack.len() + 5).into_bytes();
        reply.extend_from_slice(&pack);
        if complete {
            reply.extend_from_slice(b"0000");
        }
        reply
    }

    #[test]
    fn failed_exact_transfer_checks_which_pack_served_the_read() {
        use git2::ObjectType::Blob;

        #[derive(Debug)]
        enum Completion {
            Extra,
            CachedDecode,
            ExtraBeforeDecode,
            RetiredForecast,
        }
        for completion in [
            Completion::Extra,
            Completion::CachedDecode,
            Completion::ExtraBeforeDecode,
            Completion::RetiredForecast,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let mut scratch = RawBlobs::new(directory.path()).unwrap();
            let shared = Arc::<Shared>::default();
            let data = b"stored bytes";
            let oid = Oid::hash_object(Blob, data).unwrap();
            let retired = if matches!(completion, Completion::RetiredForecast) {
                let forecast = {
                    let mut queue = shared.queue.lock().unwrap();
                    let item = queue.insert(oid);
                    queue.ahead.push_back(item);
                    Work::take(&mut queue.ahead, true).unwrap()
                };
                let response = shared.response(&forecast, &[oid], &scratch);
                let report = shared.report_http2_failure(&forecast, &scratch);
                let old = forecast.readers[0].1.clone();
                shared.finish(
                    forecast,
                    Err(anyhow::anyhow!("unused forecast reset")),
                    Instant::now(),
                    None,
                    &scratch,
                );
                Some((old, response, report))
            } else {
                None
            };
            let (work, demand) = {
                let mut queue = shared.queue.lock().unwrap();
                let demand = queue.foreground(oid);
                let work = Work::take(&mut queue.waiting, false).unwrap();
                (work, demand)
            };
            if let Some((old, mut response, report)) = retired {
                assert!(!Arc::ptr_eq(&old.0, &demand.0));
                let late = response
                    .write_http(&pack_reply(&[(Blob, data)], false))
                    .unwrap_err();
                assert!(object::is_cancelled(&late));
                report(&anyhow::anyhow!("unused forecast reset"));
                assert!(shared.queue.lock().unwrap().failure.is_none());
            } else {
                let other = b"alternate";
                let other_oid = Oid::hash_object(Blob, other).unwrap();
                let alternate = {
                    let mut queue = shared.queue.lock().unwrap();
                    queue.foreground(other_oid);
                    Work::take(&mut queue.waiting, false).unwrap()
                };
                let mut response = shared.response(&alternate, &[other_oid], &scratch);
                response
                    .write_http(&pack_reply(&[(Blob, other), (Blob, data)], true))
                    .unwrap();
                shared.finish(
                    alternate,
                    response.finish_http(),
                    Instant::now(),
                    None,
                    &scratch,
                );
                assert!(demand.ready());
                let Some(crate::raw_blob::ReceivedObject::Verified { object, transfer }) =
                    scratch.received(oid)
                else {
                    panic!("validated pack did not publish its extra");
                };
                // Leave the validated extra in the received cache when the
                // exact decoder will publish bytes for the same OID.
                if !matches!(completion, Completion::ExtraBeforeDecode) {
                    scratch
                        .insert_verified_blob(object, transfer, false, || false, |_| {})
                        .unwrap();
                }
            }
            if !matches!(completion, Completion::Extra) {
                shared
                    .response(&work, &[oid], &scratch)
                    .write_http(&pack_reply(&[(Blob, data)], false))
                    .unwrap();
                assert!(demand.ready());
            }
            let acquired = demand.wait().take().unwrap().unwrap();
            assert_eq!(
                matches!(acquired, Acquired::ValidatedExtra),
                !matches!(completion, Completion::RetiredForecast),
                "{completion:?}"
            );
            let (blob, _) = scratch.for_read(oid).unwrap().unwrap();
            assert_eq!(blob.read(0, data.len() as u32).unwrap(), data);

            const RESET: &str = "exact response was reset";
            let error = anyhow::anyhow!(RESET);
            shared.report_http2_failure(&work, &scratch)(&error);
            let early = shared.queue.lock().unwrap().failure.take();
            shared.finish(work, Err(error), Instant::now(), None, &scratch);
            let finished = shared.queue.lock().unwrap().failure.take();
            let expected = matches!(completion, Completion::RetiredForecast).then_some(RESET);
            assert_eq!(early.as_deref(), expected, "{completion:?}");
            assert_eq!(finished.as_deref(), expected, "{completion:?}");
        }
    }

    #[test]
    fn exact_tree_delivery_waits_for_validated_extra_publication() {
        use git2::ObjectType::{Blob, Tree};

        let directory = tempfile::tempdir().unwrap();
        let scratch = RawBlobs::new(directory.path()).unwrap();
        let shared = Arc::<Shared>::default();
        let tree = Oid::hash_object(Tree, b"").unwrap();
        let blob = Oid::hash_object(Blob, b"other").unwrap();
        let (exact, demand, alternate) = {
            let mut queue = shared.queue.lock().unwrap();
            let demand = queue.foreground(tree);
            let exact = Work::take(&mut queue.waiting, false).unwrap();
            queue.foreground(blob);
            let alternate = Work::take(&mut queue.waiting, false).unwrap();
            (exact, demand, alternate)
        };
        let mut response = shared.response(&alternate, &[blob], &scratch);
        response
            .write_http(&pack_reply(&[(Blob, b"other"), (Tree, b"")], true))
            .unwrap();
        let mut pack = response.finish_http().unwrap();
        let mut response = shared.response(&exact, &[tree], &scratch);
        let mut queue = shared.queue.lock().unwrap();
        let mut validated = Vec::new();
        pack.cache_extras(&scratch, None, |oid| validated.push(oid));
        assert_eq!(validated, [tree]);
        thread::scope(|scope| {
            let (started, start) = mpsc::sync_channel(0);
            let (finished, finish) = mpsc::channel();
            let worker = scope.spawn(move || {
                let body = pack_reply(&[(Tree, b"")], false);
                started.send(()).unwrap();
                let result = response.write_http(&body);
                finished.send(()).unwrap();
                result
            });
            start.recv().unwrap();
            let _ = finish.recv_timeout(Duration::from_millis(100));
            queue.complete_extra(tree);
            drop(queue);
            worker.join().unwrap().unwrap();
        });
        assert!(matches!(
            demand.wait().take(),
            Some(Ok(Acquired::ValidatedExtra))
        ));
        let error = anyhow::anyhow!("redundant tree transfer reset");
        shared.report_http2_failure(&exact, &scratch)(&error);
        shared.finish(exact, Err(error), Instant::now(), None, &scratch);
        assert!(shared.queue.lock().unwrap().failure.is_none());
    }

    #[test]
    fn failed_prefetch_does_not_treat_a_cancelled_waiter_as_served() {
        let directory = tempfile::tempdir().unwrap();
        let mut scratch = RawBlobs::new(directory.path()).unwrap();
        let shared = Arc::<Shared>::default();
        let data = b"late but no reader";
        let oid = Oid::hash_object(git2::ObjectType::Blob, data).unwrap();
        let (work, demand) = {
            let mut queue = shared.queue.lock().unwrap();
            let item = queue.insert(oid);
            queue.ahead.push_back(item);
            let work = Work::take(&mut queue.ahead, true).unwrap();
            (work, queue.foreground(oid))
        };
        let report = shared.report_http2_failure(&work, &scratch);
        let error = anyhow::anyhow!("unused forecast transfer failed");
        report(&error);
        demand.complete(Batch::new(
            vec![oid],
            Err(object::OperationCancelled.into()),
        ));
        let late = scratch.insert_fetched(oid, data.len(), &data[..], Some(work.transfer.clone()));
        assert!(late.is_err_and(|error| object::is_cancelled(&error)));
        assert!(!scratch.available(oid));
        report(&error);
        assert!(shared.queue.lock().unwrap().failure.is_none());
    }

    #[test]
    fn failed_prefetch_reports_pack_errors_after_a_real_read() {
        for joins_request in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let mut scratch = RawBlobs::new(directory.path()).unwrap();
            let shared = Arc::<Shared>::default();
            let data = vec![b'x'; crate::raw_blob::BLOCK + 1];
            let oid = Oid::hash_object(git2::ObjectType::Blob, &data).unwrap();
            let work = {
                let mut queue = shared.queue.lock().unwrap();
                let item = queue.insert(oid);
                queue.ahead.push_back(item);
                let work = Work::take(&mut queue.ahead, true).unwrap();
                if joins_request {
                    queue.foreground(oid);
                }
                work
            };
            let report = shared.report_http2_failure(&work, &scratch);
            scratch
                .insert_fetched(oid, data.len(), &data[..], Some(work.transfer.clone()))
                .unwrap();
            if joins_request {
                let demand = &work.items[0].demand;
                shared.complete_object(oid, demand, true, Acquired::Stored);
                assert!(matches!(demand.wait().take(), Some(Ok(Acquired::Stored))));
            } else {
                let (blob, first) = scratch.for_read(oid).unwrap().unwrap();
                assert!(first);
                assert_eq!(blob.read(0, 1).unwrap(), b"x");
            }
            let error = anyhow::anyhow!("Git pack checksum mismatch");
            report(&error);
            assert_eq!(
                shared.queue.lock().unwrap().failure.take().as_deref(),
                Some("Git pack checksum mismatch")
            );
            shared.complete_object(oid, &work.items[0].demand, true, Acquired::Stored);
            shared.finish(work, Err(error), Instant::now(), None, &scratch);
            let queue = shared.queue.lock().unwrap();
            assert_eq!(queue.failure.as_deref(), Some("Git pack checksum mismatch"));
            assert!(queue.waiting.is_empty());
        }
    }

    #[test]
    fn failed_unused_forecast_is_retried_on_first_read() {
        use git2::ObjectType::Blob;

        let directory = tempfile::tempdir().unwrap();
        let pool = Pool {
            shared: Arc::default(),
            scratch: RawBlobs::new(directory.path()).unwrap(),
            workers: Mutex::default(),
            wake: None,
            ssh_snapshot: None,
        };
        let data = b"verified bytes";
        let oid = Oid::hash_object(Blob, data).unwrap();
        let forecast = {
            let mut queue = pool.shared.queue.lock().unwrap();
            let item = queue.insert(oid);
            queue.ahead.push_back(item);
            Work::take(&mut queue.ahead, true).unwrap()
        };
        pool.shared
            .response(&forecast, &[oid], &pool.scratch)
            .write_http(&pack_reply(&[(Blob, data)], false))
            .unwrap();
        assert!(pool.scratch.available(oid));
        let error = anyhow::Error::from(io::Error::from(io::ErrorKind::ConnectionReset));
        let report = pool.shared.report_http2_failure(&forecast, &pool.scratch);
        report(&error);
        assert!(!pool.scratch.available(oid));
        let demand = pool.request(oid).unwrap();
        pool.shared
            .finish(forecast, Err(error), Instant::now(), None, &pool.scratch);
        assert!(pool.failure().is_none());
        assert!(!pool.scratch.available(oid));

        assert!(!demand.ready());
        let exact = Work::take(&mut pool.shared.queue.lock().unwrap().waiting, false).unwrap();
        let mut response = pool.shared.response(&exact, &[oid], &pool.scratch);
        response
            .write_http(&pack_reply(&[(Blob, data)], true))
            .unwrap();
        pool.shared.finish(
            exact,
            response.finish_http(),
            Instant::now(),
            None,
            &pool.scratch,
        );
        assert!(matches!(demand.wait().take(), Some(Ok(Acquired::Stored))));
        let (blob, _) = pool.scratch.clone().for_read(oid).unwrap().unwrap();
        assert_eq!(blob.read(0, data.len() as u32).unwrap(), data);
    }

    #[test]
    fn a_completed_forecast_tracks_a_later_read_of_its_extra() {
        use crate::raw_blob::ReceivedObject;
        use git2::ObjectType::Blob;

        let directory = tempfile::tempdir().unwrap();
        let mut scratch = RawBlobs::new(directory.path()).unwrap();
        let shared = Arc::<Shared>::default();
        let requested = Oid::hash_object(Blob, b"forecast").unwrap();
        let extra = Oid::hash_object(Blob, b"extra").unwrap();
        let forecast = {
            let mut queue = shared.queue.lock().unwrap();
            let item = queue.insert(requested);
            queue.ahead.push_back(item);
            Work::take(&mut queue.ahead, true).unwrap()
        };
        let active = forecast.active_pack();
        let mut response = shared.response(&forecast, &[requested], &scratch);
        response
            .write_http(&pack_reply(&[(Blob, b"forecast"), (Blob, b"extra")], true))
            .unwrap();
        shared.finish(
            forecast,
            response.finish_http(),
            Instant::now(),
            None,
            &scratch,
        );
        assert!(!active.supplied_data(&scratch));
        let mut queue = shared.queue.lock().unwrap();
        let error = anyhow::anyhow!("SSH channel exited with code 49");
        queue.record_channel_failure(false, &mut vec![active.clone()], &error, &scratch);
        assert!(queue.recorded_failure().is_none());
        let Some(ReceivedObject::Verified { object, transfer }) = scratch.received(extra) else {
            panic!("verified extra missing");
        };
        scratch
            .insert_verified_blob(object, transfer, false, || false, |_| {})
            .unwrap();
        let (blob, _) = scratch.for_read(extra).unwrap().unwrap();
        assert_eq!(blob.read(0, 5).unwrap(), b"extra");
        assert!(active.supplied_data(&scratch));
        assert_eq!(
            queue.late_failure.get().map(AsRef::as_ref),
            Some("SSH channel exited with code 49")
        );
        assert_eq!(
            queue.recorded_failure(),
            Some("SSH channel exited with code 49")
        );
    }

    #[test]
    fn another_completed_pack_confirms_an_unused_forecast() {
        use git2::ObjectType::Blob;

        let directory = tempfile::tempdir().unwrap();
        let scratch = RawBlobs::new(directory.path()).unwrap();
        let shared = Arc::<Shared>::default();
        let data = b"foreseen";
        let oid = Oid::hash_object(Blob, data).unwrap();
        let other = Oid::hash_object(Blob, b"requested").unwrap();
        let (forecast, alternate) = {
            let mut queue = shared.queue.lock().unwrap();
            let item = queue.insert(oid);
            queue.ahead.push_back(item);
            queue.foreground(other);
            (
                Work::take(&mut queue.ahead, true).unwrap(),
                Work::take(&mut queue.waiting, false).unwrap(),
            )
        };
        shared
            .response(&forecast, &[oid], &scratch)
            .write_http(&pack_reply(&[(Blob, data)], false))
            .unwrap();
        assert!(scratch.available(oid));
        let mut response = shared.response(&alternate, &[other], &scratch);
        response
            .write_http(&pack_reply(&[(Blob, b"requested"), (Blob, data)], true))
            .unwrap();
        shared.finish(
            alternate,
            response.finish_http(),
            Instant::now(),
            None,
            &scratch,
        );
        let (blob, _) = scratch.clone().for_read(oid).unwrap().unwrap();
        assert_eq!(blob.read(0, data.len() as u32).unwrap(), data);
        shared.finish(
            forecast,
            Err(io::Error::from(io::ErrorKind::ConnectionReset).into()),
            Instant::now(),
            None,
            &scratch,
        );
        assert!(shared.queue.lock().unwrap().failure.is_none());
    }

    #[test]
    fn ready_forecasts_are_not_submitted_with_waiting_work() {
        let mut queue = Queue::default();
        let cached = Oid::from_bytes(&[1; 20]).unwrap();
        let missing = Oid::from_bytes(&[2; 20]).unwrap();
        // A foreground request can claim verified bytes from a retiring
        // forecast after its own item entered the wait queue.
        let demand = queue.foreground(cached);
        demand.complete(Batch::new(vec![cached], Ok(Acquired::Stored)));
        queue.foreground(missing);
        let work = Work::take(&mut queue.waiting, false).unwrap();
        assert_eq!(work.wants(), [missing]);
        assert!(queue.waiting.is_empty());
        assert!(queue.pending[&cached].ready());
    }

    #[test]
    fn pending_demand_reuses_a_blob_admitted_before_its_claim() {
        let directory = tempfile::tempdir().unwrap();
        let pool = Pool {
            shared: Arc::default(),
            scratch: RawBlobs::new(directory.path()).unwrap(),
            workers: Mutex::default(),
            wake: None,
            ssh_snapshot: None,
        };
        let data = vec![b'x'; crate::raw_blob::BLOCK + 1];
        let oid = Oid::hash_object(git2::ObjectType::Blob, &data).unwrap();
        let flight = {
            let mut queue = pool.shared.queue.lock().unwrap();
            let item = queue.insert(oid);
            let flight = item.demand.clone();
            queue.ahead.push_back(item);
            flight
        };
        pool.scratch
            .clone()
            .insert_fetched(oid, data.len(), &data[..], None)
            .unwrap();
        let mut required_during_lookup = false;
        let demand = pool
            .request_with_lookup(oid, false, || {
                required_during_lookup = flight.required();
                pool.scratch.lookup_demand(oid)
            })
            .unwrap();
        assert!(
            required_during_lookup,
            "the decoder must see the joining reader before lookup"
        );
        assert!(demand.ready());
        assert!(Work::take(&mut pool.shared.queue.lock().unwrap().waiting, false).is_none());
        assert_eq!(pool.metrics().oids, 0);
        for _ in 0..2 {
            pool.shared
                .complete_object(oid, &flight, true, Acquired::Stored);
        }
        assert_eq!(pool.metrics().oids, 1);
    }

    #[test]
    fn a_forecast_that_served_a_read_can_finish_with_all_decode_slots_busy() {
        use git2::ObjectType::Blob;

        let directory = tempfile::tempdir().unwrap();
        let mut cache = RawBlobs::new(directory.path()).unwrap();
        let shared = Arc::<Shared>::default();
        let early = b"read";
        let later = vec![b'x'; 128 * 1024];
        let oid = Oid::hash_object(Blob, early).unwrap();
        let other = Oid::hash_object(Blob, &later).unwrap();
        let work = {
            let mut queue = shared.queue.lock().unwrap();
            let mut first = queue.insert(oid);
            first.prefetch_width = 2;
            let next = queue.insert(other);
            queue.ahead.extend([first, next]);
            Work::take(&mut queue.ahead, true).unwrap()
        };
        let mut pack = shared.response(&work, &[oid, other], &cache);
        let reply = pack_reply(&[(Blob, early), (Blob, &later)], true);
        let split = pack_reply(&[(Blob, early)], false).len();
        pack.write_http(&reply[..split]).unwrap();
        let (read, _) = cache.for_read(oid).unwrap().unwrap();
        assert_eq!(read.read(0, 4).unwrap(), early);

        let held: Vec<_> = (0..7)
            .map(|_| {
                cache
                    .reserve_pack_decode(
                        || crate::raw_blob::PackDecodePriority::Speculative,
                        object::check_transfer,
                    )
                    .unwrap()
            })
            .collect();
        let ordinary = cache
            .reserve_pack_decode(
                || crate::raw_blob::PackDecodePriority::Demand,
                object::check_transfer,
            )
            .unwrap();
        let verification = PackVerification::new(&cache);
        verification.pool.lock().unwrap().shared = Arc::downgrade(&shared);
        let unrelated = shared
            .queue
            .lock()
            .unwrap()
            .foreground(Oid::from_bytes(&[23; 20]).unwrap());
        let (starting, started) = mpsc::channel();
        let (finished, result) = mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(move || {
                starting.send(()).unwrap();
                let outcome = pack
                    .write_http(&reply[split..])
                    .and_then(|()| pack.finish_http());
                shared.finish(work, outcome, Instant::now(), None, &cache);
                finished.send(()).unwrap();
            });
            let began = started.recv_timeout(Duration::from_secs(2));
            let early = result.recv_timeout(Duration::from_millis(150));
            let (checked, check) = mpsc::channel();
            scope.spawn(move || checked.send(verification.wait()).unwrap());
            let verified = check.recv_timeout(Duration::from_secs(2));
            drop(held);
            drop(ordinary);
            began.unwrap();
            assert!(
                matches!(early, Err(mpsc::RecvTimeoutError::Timeout)),
                "forecast reserved the last slot while the command could still read"
            );
            verified
                .expect("a pack that served the command still needs its trailer")
                .unwrap();
            assert!(
                unrelated.ready(),
                "command exit left an unrelated reader queued"
            );
        });
    }

    #[test]
    fn idle_ssh_sends_eof_while_a_used_pack_is_still_being_verified() -> Result<()> {
        use git2::ObjectType::Blob;
        use std::process::Command;

        let directory = tempfile::tempdir()?;
        let cache = RawBlobs::new(directory.path())?;
        let connection = Connection::new(
            Command::new("/usr/bin/false"),
            false,
            Some(crate::transport::SshSocket {
                // The worker already owns its channel. No new fetch in this
                // test can open a connection.
                path: directory.path().join("unused-control-socket"),
                device: 0,
                inode: 0,
            }),
        );
        let eof = directory.path().join("received-eof");
        let contents = b"completed";
        let oid = Oid::hash_object(Blob, contents)?;
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                r#"
                printf '000eversion 2\n0019fetch=shallow filter\n0000'
                while IFS= read -r packet; do
                    case "$packet" in *done) break;; esac
                done
                printf '%b' "$1"
                cat >/dev/null
                : >"$2"
                "#,
                "--",
            ])
            .arg(
                pack_reply(&[(Blob, contents)], true)
                    .iter()
                    .map(|byte| format!("\\0{byte:03o}"))
                    .collect::<String>(),
            )
            .arg(&eof);
        let mut idle =
            Session::connect(command, false)?.context("idle SSH fixture must negotiate")?;
        let response = pack_stream::Response::new(cache.clone(), vec![oid], Box::new(|_| Ok(())));
        idle.fetch_stream(&[oid], crate::transport::FetchMode::Filtered, response)?;
        let mut pool = Pool::prepare_native(&connection, &cache, vec![])?;
        pool.attach_primary(idle, connection)?;

        let pending = b"served, trailer still pending";
        let oid = Oid::hash_object(Blob, pending)?;
        let work = {
            let mut queue = pool.shared.queue.lock().unwrap();
            queue.foreground(oid);
            Work::take(&mut queue.waiting, false).unwrap()
        };
        let mut pack = pool.shared.response(&work, &[oid], &cache);
        let reply = pack_reply(&[(Blob, pending)], true);
        let trailer = reply.len() - 24;
        pack.write_http(&reply[..trailer])?;
        let (blob, _) = cache
            .clone()
            .for_read(oid)?
            .context("verified blob before trailer")?;
        assert_eq!(blob.read(0, pending.len() as u32)?, pending);
        let verification = PackVerification::new(&cache);
        verification.observe(&pool);
        thread::scope(|scope| {
            let verifier = scope.spawn(|| verification.wait());
            let deadline = Instant::now() + Duration::from_secs(2);
            while !eof.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            let early = eof.exists();
            let verified = verifier.is_finished();
            let outcome = pack
                .write_http(&reply[trailer..])
                .and_then(|()| pack.finish_http());
            pool.shared
                .finish(work, outcome, Instant::now(), None, &cache);
            verifier.join().unwrap()?;
            pool.stop()?;
            assert!(!verified, "the verification ignored the held pack trailer");
            assert!(
                early,
                "the idle SSH channel waited for an unrelated pack trailer before sending EOF"
            );
            Ok(())
        })
    }

    #[test]
    fn only_persistent_channel_failure_affects_a_prior_completed_response() -> Result<()> {
        for stateless in [false, true] {
            check_exit_after_unused_fetch(stateless, false)?;
            check_exit_after_unused_fetch(stateless, true)?;
        }
        Ok(())
    }

    fn check_exit_after_unused_fetch(stateless: bool, signaled: bool) -> Result<()> {
        use git2::ObjectType::Blob;
        use std::process::Command;

        let directory = tempfile::tempdir()?;
        let scratch = RawBlobs::new(directory.path())?;
        let contents = b"consumed";
        let oid = Oid::hash_object(Blob, contents)?;
        let unused = Oid::hash_object(Blob, b"unused forecast")?;
        let encode = |bytes: &[u8]| {
            bytes
                .iter()
                .map(|byte| format!("\\0{byte:03o}"))
                .collect::<String>()
        };
        let pid_file = directory.path().join("session-pid");
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                r#"
            printf '%s\n' "$$" >"$3"
            if [ "$4" = "stateless" ]; then
                IFS= read -r capabilities
                printf 'stateless-connect\n\n'
                IFS= read -r connect
                printf '\n'
            fi
            printf '%b' "$1"
            while IFS= read -r packet; do
                case "$packet" in *done) break;; esac
            done
            printf '%b' "$2"
            if [ "$4" = "stateless" ]; then
                printf '0002'
            fi
            if [ "$5" = "signal" ]; then
                kill -TERM "$$"
            fi
            exit 49
        "#,
                "--",
            ])
            .arg(encode(b"000eversion 2\n0019fetch=shallow filter\n0000"))
            .arg(encode(&pack_reply(&[(Blob, contents)], true)))
            .arg(&pid_file)
            .arg(if stateless { "stateless" } else { "persistent" })
            .arg(if signaled { "signal" } else { "exit" });
        let session = Session::connect(command, stateless)?
            .context("local transport fixture must negotiate")?;
        let connection = Connection::new(Command::new("/usr/bin/false"), stateless, None);
        let mut pool = Pool {
            shared: Arc::default(),
            scratch: scratch.clone(),
            workers: Mutex::default(),
            wake: None,
            ssh_snapshot: None,
        };
        pool.attach_primary(session, connection)?;
        let demand = pool.request(oid)?;
        assert!(matches!(demand.wait().take(), Some(Ok(Acquired::Stored))));
        let (blob, _) = scratch.clone().for_read(oid)?.unwrap();
        assert_eq!(blob.read(0, contents.len() as u32)?, contents);

        let pid = std::fs::read_to_string(pid_file)?.trim().parse()?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while !crate::transport::child_exited(pid)? {
            assert!(Instant::now() < deadline, "fixture did not exit");
            thread::sleep(Duration::from_millis(5));
        }
        pool.prefetch(oid, |_| std::iter::once(unused), |_| false);
        {
            let queue = pool.shared.queue.lock().unwrap();
            let (queue, timer) = pool
                .shared
                .work
                .wait_timeout_while(queue, Duration::from_secs(3), |queue| {
                    !queue.ahead.is_empty() || queue.busy[0]
                })
                .unwrap();
            assert!(!timer.timed_out(), "follow-up forecast was not processed");
            assert!(queue.predicted.contains_key(&unused));
        }
        let verification = PackVerification::new(&scratch);
        verification.observe(&pool);
        let outcome = verification.wait();
        if stateless {
            outcome?;
        } else {
            let error = outcome.unwrap_err();
            let expected = if signaled {
                "signal: 15"
            } else {
                "exit status: 49"
            };
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        }
        Ok(())
    }

    #[test]
    fn a_satisfied_forecast_leaves_the_foreground_decode_buffer_available() {
        let directory = tempfile::tempdir().unwrap();
        let cache = RawBlobs::new(directory.path()).unwrap();
        let held: Vec<_> = (0..7)
            .map(|_| {
                cache
                    .reserve_pack_decode(
                        || crate::raw_blob::PackDecodePriority::Speculative,
                        object::check_transfer,
                    )
                    .unwrap()
            })
            .collect();
        let shared = Arc::<Shared>::default();
        let oid = Oid::from_bytes(&[1; 20]).unwrap();
        let work = {
            let mut queue = shared.queue.lock().unwrap();
            let item = queue.insert(oid);
            queue.ahead.push_back(item);
            let work = Work::take(&mut queue.ahead, true).unwrap();
            let demand = queue.foreground(oid);
            demand.complete(Batch::new(vec![oid], Ok(Acquired::Stored)));
            work
        };
        let mut pack = shared.response(&work, &[oid], &cache);
        let (start, started) = mpsc::channel();
        let (write, written) = mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(move || {
                start.send(()).unwrap();
                write
                    .send(
                        pack.write_http(
                            b"000dpackfile\n0014\x01PACK\0\0\0\x02\0\0\0\x01\xb1\x80\x20",
                        ),
                    )
                    .unwrap();
            });
            let began = started.recv_timeout(Duration::from_secs(3));
            let attempted = written.recv_timeout(Duration::from_millis(150));
            drop(held);
            began.unwrap();
            assert!(matches!(attempted, Err(mpsc::RecvTimeoutError::Timeout)));
            written
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
        });
    }

    #[test]
    fn a_ready_channel_can_take_demand_while_another_channel_opens() {
        let mut queue = Queue::default();
        let oid = Oid::from_bytes(&[1; 20]).unwrap();
        queue.foreground(oid);
        queue.busy[0] = true;
        assert!(matches!(
            queue.native_work(1, true),
            Some(NativeWork::Connect)
        ));
        assert!(queue.native_work(2, true).is_none());
        queue.busy[0] = false;
        let Some(NativeWork::Fetch {
            work,
            failure: None,
            ..
        }) = queue.native_work(0, false)
        else {
            panic!("the ready channel did not receive the pending demand");
        };
        assert_eq!(work.wants(), [oid]);
        assert!(queue.waiting.is_empty());
    }

    #[test]
    fn one_shot_ssh_requests_leave_a_negotiated_channel_available() {
        let mut candidate = std::process::Command::new("/bin/cat")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let ready = SshChannel::Ready(candidate.id());
        let mut queue = Queue {
            mode: ConnectionMode::Probing(1),
            mount_reads: MountReads::Lookup,
            initial_channels: INITIAL_CONNECTIONS + 1,
            ..Queue::default()
        };
        queue.ssh_channels[0] = ready;
        queue.ssh_channels[INITIAL_CONNECTIONS] = ready;
        queue.foreground(Oid::from_bytes(&[1; 20]).unwrap());
        assert!(matches!(
            queue.native_work(0, false),
            Some(NativeWork::Fetch {
                last_request: true,
                ..
            })
        ));
        queue.busy[0] = false;
        queue.foreground(Oid::from_bytes(&[2; 20]).unwrap());
        assert!(queue.native_work(0, true).is_none());
        assert!(matches!(
            queue.native_work(INITIAL_CONNECTIONS, false),
            Some(NativeWork::Fetch {
                last_request: false,
                ..
            })
        ));

        queue.mount_reads = MountReads::Enumeration;
        queue.busy.fill(false);
        queue.ssh_channels[0] = ready;
        queue.foreground(Oid::from_bytes(&[3; 20]).unwrap());
        assert!(matches!(
            queue.native_work(0, false),
            Some(NativeWork::Fetch {
                last_request: false,
                ..
            })
        ));
        drop(candidate.stdin.take());
        assert!(candidate.wait().unwrap().success());
    }

    #[test]
    fn optional_ssh_channel_rejection_leaves_a_foreground_retry() {
        let mut queue = Queue::default();
        queue.ssh_channels[0] = SshChannel::Spent;
        let forecast = queue.insert(Oid::from_bytes(&[1; 20]).unwrap());
        queue.ahead.push_back(forecast);
        assert!(matches!(
            queue.native_work(0, true),
            Some(NativeWork::Connect)
        ));
        queue.busy[0] = false;
        queue.rejected_ssh_channel(0, anyhow::anyhow!("additional SSH session rejected"));
        assert!(queue.failure.is_none());
        queue.foreground(Oid::from_bytes(&[2; 20]).unwrap());
        assert!(matches!(
            queue.native_work(0, true),
            Some(NativeWork::Connect)
        ));
    }

    #[test]
    fn a_foreground_ssh_reconnect_can_use_an_extra_startup_slot() {
        let mut queue = Queue {
            initial_channels: INITIAL_CONNECTIONS + 1,
            ..Queue::default()
        };
        queue.ssh_channels[..INITIAL_CONNECTIONS].fill(SshChannel::Unavailable);
        queue.starting[INITIAL_CONNECTIONS] = true;
        queue.foreground(Oid::from_bytes(&[1; 20]).unwrap());
        assert!(queue.native_work(0, true).is_none());
        queue.starting[INITIAL_CONNECTIONS] = false;
        assert!(matches!(
            queue.native_work(INITIAL_CONNECTIONS, true),
            Some(NativeWork::Connect)
        ));
    }

    #[test]
    fn abandoned_ssh_connection_attempts_do_not_fail_the_command() {
        let failed = [false, true].map(|cancelled| {
            let mut queue = Queue::default();
            queue.ssh_channels[0] = SshChannel::Spent;
            queue.foreground(Oid::from_bytes(&[1; 20]).unwrap());
            assert!(matches!(
                queue.native_work(0, true),
                Some(NativeWork::Connect)
            ));
            let error = if cancelled {
                object::OperationCancelled.into()
            } else {
                queue.cancel_queued();
                anyhow::anyhow!("additional SSH session rejected")
            };
            queue.busy[0] = false;
            queue.rejected_ssh_channel(0, error);
            queue.failure.is_some()
        });
        assert_eq!(failed, [false, false]);
    }

    #[test]
    fn cancelled_ssh_connection_completes_the_waiting_demand() {
        let mut queue = Queue::default();
        let demand = queue.foreground(Oid::zero());
        assert!(matches!(
            queue.native_work(0, true),
            Some(NativeWork::Connect)
        ));
        queue.busy[0] = false;
        queue.rejected_ssh_channel(0, object::OperationCancelled.into());
        assert!(demand.ready());
        assert!(matches!(
            demand.wait().take(),
            Some(Err(error)) if object::is_cancelled(&error)
        ));
        assert!(queue.native_work(0, true).is_none());
    }

    #[test]
    fn a_validated_extra_does_not_open_an_ssh_channel() {
        let mut queue = Queue::default();
        queue.foreground(Oid::zero());
        queue.complete_extra(Oid::zero());
        assert!(queue.native_work(0, true).is_none());
    }

    #[test]
    fn opened_ssh_channel_can_serve_demand_after_the_opening_limit_shrinks() {
        let mut queue = Queue {
            mode: ConnectionMode::Standard,
            initial_channels: INITIAL_CONNECTIONS - 1,
            ..Queue::default()
        };
        queue.ssh_channels[..INITIAL_CONNECTIONS].fill(SshChannel::Unavailable);
        let ready = INITIAL_CONNECTIONS;
        queue.ssh_channels[ready] = SshChannel::Ready(std::process::id());
        queue.foreground(Oid::zero());
        let Some(NativeWork::Fetch { work, .. }) = queue.native_work(ready, false) else {
            panic!("an established channel can serve exact requests");
        };
        queue.ssh_channels[ready] = SshChannel::Spent;
        queue.busy[ready] = false;
        for item in work.items {
            queue.retry(item, false);
        }
        assert!(matches!(
            queue.native_work(ready, true),
            Some(NativeWork::Connect)
        ));
    }

    #[test]
    fn ssh_recovery_waits_for_the_only_live_channel_to_release_its_slot() {
        let mut queue = Queue {
            mode: ConnectionMode::Standard,
            initial_channels: INITIAL_CONNECTIONS,
            ..Queue::default()
        };
        queue.ssh_channels[..INITIAL_CONNECTIONS].fill(SshChannel::Unavailable);
        let last = INITIAL_CONNECTIONS;
        queue.ssh_channels[last] = SshChannel::Ready(std::process::id());
        queue.busy[last] = true;
        queue.foreground(Oid::zero());
        assert!(queue.native_work(last + 1, true).is_none());
        queue.ssh_channels[last] = SshChannel::Spent;
        assert!(queue.native_work(last + 1, true).is_none());
        queue.busy[last] = false;
        assert!(matches!(
            queue.native_work(last, true),
            Some(NativeWork::Connect)
        ));
    }

    #[test]
    fn ssh_recovery_can_reopen_a_released_slot_while_another_pack_is_active() {
        let mut queue = Queue {
            mode: ConnectionMode::Standard,
            ..Queue::default()
        };
        queue.ssh_channels[..INITIAL_CONNECTIONS].fill(SshChannel::Unavailable);
        let unopened = INITIAL_CONNECTIONS;
        let released = unopened + 1;
        let active = released + 1;
        queue.ssh_channels[released] = SshChannel::Spent;
        queue.ssh_channels[active] = SshChannel::Ready(std::process::id());
        queue.busy[active] = true;
        let demand = queue.foreground(Oid::zero());

        assert!(queue.native_work(unopened, true).is_none());
        assert!(matches!(
            queue.native_work(released, true),
            Some(NativeWork::Connect)
        ));
        assert!(!demand.ready());
    }

    #[test]
    fn ssh_recovery_waits_for_standby_progress_only_after_a_refused_reopen() {
        let mut queue = Queue {
            mode: ConnectionMode::Standard,
            ..Queue::default()
        };
        queue.ssh_channels[..INITIAL_CONNECTIONS].fill(SshChannel::Unavailable);
        let ordinary_standby = INITIAL_CONNECTIONS - 1;
        queue.ssh_channels[ordinary_standby] = SshChannel::Unopened;
        queue.starting[ordinary_standby] = true;
        let released = INITIAL_CONNECTIONS;
        queue.ssh_channels[released] = SshChannel::Spent;
        queue.starting[released + 1] = true;
        queue.foreground(Oid::zero());

        assert!(matches!(
            queue.native_work(released, true),
            Some(NativeWork::Connect)
        ));
        queue.busy[released] = false;
        queue.rejected_ssh_channel(released, anyhow::anyhow!("SSH reopen refused"));
        assert!(queue.failure.is_none());
        assert!(queue.native_work(released, true).is_none());
        queue.finished_ssh_standby(released + 1);
        assert!(matches!(
            queue.native_work(released, true),
            Some(NativeWork::Connect)
        ));
    }

    #[test]
    fn ssh_recovery_can_bypass_a_negotiating_worker_within_the_opening_limit() {
        let mut queue = Queue {
            mode: ConnectionMode::Standard,
            ..Queue::default()
        };
        queue.ssh_channels[0] = SshChannel::Unavailable;
        queue.starting[1] = true;
        queue.ssh_channels[2] = SshChannel::Spent;
        queue.foreground(Oid::zero());

        assert!(matches!(
            queue.native_work(2, true),
            Some(NativeWork::Connect)
        ));
    }

    #[test]
    fn ssh_recovery_can_retry_if_a_standby_finished_during_the_refused_open() {
        let mut queue = Queue {
            mode: ConnectionMode::Standard,
            ..Queue::default()
        };
        queue.ssh_channels[..INITIAL_CONNECTIONS].fill(SshChannel::Unavailable);
        let released = INITIAL_CONNECTIONS;
        queue.ssh_channels[released] = SshChannel::Spent;
        queue.starting[released + 1] = true;
        queue.foreground(Oid::zero());

        assert!(matches!(
            queue.native_work(released, true),
            Some(NativeWork::Connect)
        ));
        queue.finished_ssh_standby(released + 1);
        queue.busy[released] = false;
        queue.rejected_ssh_channel(released, anyhow::anyhow!("SSH reopen refused"));
        assert!(queue.failure.is_none());
        assert!(matches!(
            queue.native_work(released, true),
            Some(NativeWork::Connect)
        ));
        queue.busy[released] = false;
        queue.rejected_ssh_channel(released, anyhow::anyhow!("SSH reopen still refused"));
        assert!(queue.failure.is_some());
    }

    #[test]
    fn ssh_refusal_preserves_the_retry_on_a_just_closed_channel() {
        let mut queue = Queue {
            mode: ConnectionMode::Wide,
            ..Queue::default()
        };
        queue.ssh_channels[0] = SshChannel::Spent;
        queue.busy[0] = true;
        queue.foreground(Oid::zero());
        assert!(matches!(
            queue.native_work(1, true),
            Some(NativeWork::Connect)
        ));
        queue.busy[..2].fill(false);
        queue.rejected_ssh_channel(1, anyhow::anyhow!("additional SSH session rejected"));
        assert!(queue.failure.is_none());
        assert!(matches!(
            queue.native_work(0, true),
            Some(NativeWork::Connect)
        ));
    }

    #[test]
    fn a_failed_lower_standby_does_not_block_a_ready_channel() {
        let mut queue = Queue {
            mode: ConnectionMode::Standard,
            initial_channels: INITIAL_CONNECTIONS + 2,
            ..Queue::default()
        };
        queue.busy[..INITIAL_CONNECTIONS].fill(true);
        let standby = INITIAL_CONNECTIONS + 1;
        queue.ssh_channels[standby] = SshChannel::Ready(std::process::id());
        queue.foreground(Oid::zero());
        assert!(queue.native_work(INITIAL_CONNECTIONS, true).is_none());
        assert!(matches!(
            queue.native_work(standby, false),
            Some(NativeWork::Fetch { .. })
        ));
    }

    #[test]
    fn exact_demand_opens_additional_channels_beyond_the_initial_pool() {
        let mut queue = Queue {
            mode: ConnectionMode::Wide,
            ..Queue::default()
        };
        queue.busy[..INITIAL_CONNECTIONS].fill(true);
        let next = queue.insert(Oid::from_bytes(&[1; 20]).unwrap());
        queue.ahead.push_back(next);
        assert!(queue.native_work(INITIAL_CONNECTIONS, true).is_none());
        queue.foreground(Oid::from_bytes(&[2; 20]).unwrap());
        assert!(matches!(
            queue.native_work(INITIAL_CONNECTIONS, true),
            Some(NativeWork::Connect)
        ));
    }

    #[test]
    fn required_http2_tree_does_not_wait_for_batching() {
        let directory = tempfile::tempdir().unwrap();
        let pool = Pool {
            shared: Arc::default(),
            scratch: RawBlobs::new(directory.path()).unwrap(),
            workers: Mutex::default(),
            wake: None,
            ssh_snapshot: None,
        };
        pool.shared.queue.lock().unwrap().mode = ConnectionMode::Wide;
        let tree = Oid::hash_object(git2::ObjectType::Tree, b"").unwrap();
        pool.request(tree).unwrap();
        {
            let queue = pool.shared.queue.lock().unwrap();
            let now = queue.waiting[0].queued_at;
            assert_eq!(queue.http2_batch_wait(now, true), Some(BATCH_WINDOW));
        }
        pool.request_with_batching(tree, true).unwrap();
        let queue = pool.shared.queue.lock().unwrap();
        assert_eq!(queue.waiting.len(), 1);
        let now = queue.waiting[0].queued_at;
        assert_eq!(queue.http2_batch_wait(now, true), None);
        assert_eq!(queue.batch_wait(now, true), Some(BATCH_WINDOW));
    }

    #[test]
    fn batch_waits_for_more_demands_until_target_or_deadline() {
        let now = Instant::now();
        let mut queue = Queue::default();
        queue.waiting.push_back(QueuedDemand {
            oid: Oid::zero(),
            demand: Demand::default(),
            queued_at: now,
            prefetch_width: READ_AHEAD,
            immediate: false,
        });
        assert_eq!(queue.batch_wait(now, true), None);
        queue.mode = ConnectionMode::Wide;
        assert_eq!(queue.batch_wait(now, true), Some(BATCH_WINDOW));
        assert_eq!(queue.batch_wait(now + BATCH_WINDOW, true), None);
        assert_eq!(queue.batch_wait(now, false), None);
        for index in 1..BATCH_TARGET {
            assert_eq!(queue.batch_wait(now, true), Some(BATCH_WINDOW));
            queue.waiting.push_back(QueuedDemand {
                oid: Oid::from_bytes(&[index as u8; 20]).unwrap(),
                demand: Demand::default(),
                queued_at: now,
                prefetch_width: READ_AHEAD,
                immediate: false,
            });
        }
        assert_eq!(queue.batch_wait(now, true), None);

        queue.mode = ConnectionMode::Standard;
        queue.waiting.truncate(1);
        assert_eq!(queue.batch_wait(now, true), None);
        queue.forecast_score = DENSE_SCORE;
        let delay = queue.batch_wait(now, true);
        assert_eq!(delay, Some(BATCH_WINDOW));
        assert!(queue.wait_for_http2_event(delay, true));
        assert!(!queue.wait_for_http2_event(delay, false));
        assert_eq!(queue.batch_wait(now, false), None);
    }
}
