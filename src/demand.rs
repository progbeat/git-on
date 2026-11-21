//! One request queue for HTTP/2, native HTTPS helpers and owned SSH channels.
use crate::{
    object, pack_stream,
    raw_blob::{DemandLookup, RawBlobs},
    transport::{BranchQuery, Connection, Session, StartingSession},
};
use anyhow::{bail, Context, Result};
use git2::Oid;
use std::{
    collections::{HashMap, VecDeque},
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Condvar, Mutex,
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

    fn needs_pack(
        &self,
        oid: Oid,
        cache: &RawBlobs,
        transfer: &Arc<()>,
        speculative: bool,
    ) -> bool {
        let state = self.0.state.lock().unwrap();
        if !speculative
            && !state
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
}

struct Work {
    items: Vec<QueuedDemand>,
    readers: Arc<[(Oid, Demand)]>,
    extras: Arc<Mutex<Option<Vec<QueuedDemand>>>>,
    // A different pack can satisfy an exact request while it is in flight.
    // Attribute early reads to the transfer whose bytes actually served them.
    transfer: Arc<()>,
    speculative: bool,
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
            transfer: Arc::new(()),
            speculative,
        })
    }

    fn wants(&self) -> Vec<Oid> {
        self.items.iter().map(|item| item.oid).collect()
    }
}

enum ConnectionMode {
    Probing(u8),
    Standard,
    Wide,
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

#[derive(Default)]
struct Queue {
    waiting: VecDeque<QueuedDemand>,
    ahead: VecDeque<QueuedDemand>,
    pending: HashMap<Oid, Demand>,
    predicted: HashMap<Oid, u64>,
    forecast_step: u64,
    forecast_score: u8,
    forecast_misses: u8,
    forecast_expanded: bool,
    busy: [bool; CONNECTIONS],
    initial_channels: usize,
    starting: [bool; CONNECTIONS],
    connecting: bool,
    native_ceiling: Option<usize>,
    failure: Option<String>,
    mode: ConnectionMode,
}

impl Queue {
    fn insert(&mut self, oid: Oid) -> QueuedDemand {
        let demand = Demand::default();
        self.pending.insert(oid, demand.clone());
        QueuedDemand {
            oid,
            demand,
            queued_at: Instant::now(),
            prefetch_width: READ_AHEAD,
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

    fn wait_for_http2_event(&self, delay: Option<Duration>, transfer_active: bool) -> bool {
        if delay.is_some() {
            transfer_active || !matches!(self.mode, ConnectionMode::Standard)
        } else {
            self.waiting.is_empty()
        }
    }

    fn native_work(&mut self, index: usize, needs_ssh_channel: bool) -> Option<NativeWork> {
        let speculative = self.waiting.is_empty();
        let ready_for_exact = !speculative && !needs_ssh_channel;
        let channels = if matches!(self.mode, ConnectionMode::Probing(_)) && self.dense() {
            CONNECTIONS
        } else {
            self.mode.channels()
        }
        .max(if ready_for_exact {
            self.initial_channels
        } else {
            0
        })
        .min(self.native_ceiling.unwrap_or(CONNECTIONS));
        // An earlier standby cannot claim work while negotiating. Let a
        // prepared channel serve exact work without opening another channel.
        if index >= channels
            || self.busy[..index]
                .iter()
                .zip(&self.starting)
                .any(|(&busy, &starting)| !(busy || ready_for_exact && starting))
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
            if pending.is_empty() || self.connecting || speculative && index >= INITIAL_CONNECTIONS
            {
                return None;
            }
            // A new channel can take several round trips. Keep the queued
            // objects available while an already open channel might free up.
            self.connecting = true;
            NativeWork::Connect
        } else {
            NativeWork::Fetch(Work::take(pending, speculative)?, self.failure.clone())
        };
        self.busy[index] = true;
        Some(work)
    }

    fn retry(&mut self, mut item: QueuedDemand, speculative: bool) {
        if !speculative || item.demand.required() {
            item.queued_at = Instant::now();
            self.waiting.push_back(item);
        } else {
            self.pending.remove(&item.oid);
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

    fn prune_prefetch(&mut self) {
        if self.native_ceiling == Some(1) {
            for item in self.ahead.drain(..) {
                self.pending.remove(&item.oid);
            }
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
    Fetch(Work, Option<String>),
}

pub(crate) struct Pool {
    shared: Arc<Shared>,
    scratch: RawBlobs,
    workers: Mutex<Vec<JoinHandle<Result<()>>>>,
    wake: Option<Arc<UnixStream>>,
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
        };
        {
            let initial = 1 + standby.len().min(CONNECTIONS - 1);
            let mut queue = pool.shared.queue.lock().unwrap();
            queue.initial_channels = initial;
            queue.starting[1..initial].fill(true);
        }
        let mut standby = standby.into_iter();
        for index in 1..CONNECTIONS {
            pool.start(index, None, connection.clone(), standby.next())?;
        }
        Ok(pool)
    }

    pub(crate) fn attach_primary(
        &mut self,
        mut first: Session,
        connection: Connection,
    ) -> Result<()> {
        first.set_stop(self.shared.stop.clone());
        self.start(0, Some(first), connection, None)
    }

    pub(crate) fn http2(
        configuration: crate::http2::Configuration,
        branch: Option<BranchQuery>,
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
                    branch.as_ref(),
                );
                let Ok(Some((client, selected))) = client else {
                    // Discovery sends no wants, so using native Git is safe here.
                    let _ = ready.send(None);
                    return Ok(());
                };
                if ready.send(Some(selected)).is_ok() {
                    worker_state.run_http2(client, read, worker_signal, &worker_scratch);
                }
                Ok(())
            })?;
        let pool = Self {
            shared,
            scratch: scratch.clone(),
            workers: Mutex::new(vec![worker]),
            wake: Some(signal),
        };
        let ready = result
            .recv()
            .context("Git HTTP/2 dispatcher stopped during discovery")?;
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
    ) -> Result<()> {
        let shared = self.shared.clone();
        let scratch = self.scratch.clone();
        let worker = thread::Builder::new()
            .name(format!("git-on-native-{index}"))
            .spawn(move || shared.worker(index, session, connection, &scratch, standby))?;
        self.workers.get_mut().unwrap().push(worker);
        Ok(())
    }

    pub(crate) fn request(&self, oid: Oid) -> Result<Demand> {
        self.request_with_lookup(oid, || self.scratch.lookup_demand(oid))
    }

    fn request_with_lookup(
        &self,
        oid: Oid,
        lookup: impl FnOnce() -> DemandLookup,
    ) -> Result<Demand> {
        object::check_transfer()?;
        let mut queue = self.shared.queue.lock().unwrap();
        if self.shared.stop.load(Ordering::Relaxed) {
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
        self.shared.queue.lock().unwrap().failure.clone()
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
        let waiting = {
            let mut queue = self.shared.queue.lock().unwrap();
            self.shared.stop.store(true, Ordering::Relaxed);
            std::mem::take(&mut queue.waiting)
                .into_iter()
                .chain(std::mem::take(&mut queue.ahead))
        };
        self.wake();
        for item in waiting {
            item.demand.complete(Batch::new(
                vec![item.oid],
                Err(object::OperationCancelled.into()),
            ));
        }
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
        let shared = self.clone();
        let mut cache = scratch.clone();
        let transfer = work.transfer.clone();
        let speculative = work.speculative;
        let tasks = work.readers.clone();
        let forecast = speculative.then(|| tasks.clone());
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
                        Some(Arc::downgrade(&transfer)),
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
        if let Some(forecast) = forecast {
            let extras = work.extras.clone();
            response.set_required(move |remaining| {
                forecast.iter().any(|(oid, demand)| {
                    demand.required() && remaining.contains(oid) && !demand.ready()
                }) || extras.lock().unwrap().as_deref().is_some_and(|extras| {
                    extras
                        .iter()
                        .any(|item| item.demand.required() && !item.demand.ready())
                })
            });
        }
        response
    }

    fn join_pack_extra(&self, extras: &Mutex<Option<Vec<QueuedDemand>>>, oid: Oid) {
        let mut queue = self.queue.lock().unwrap();
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
        let mut validated = Vec::new();
        let result = result.map(|mut pack| {
            pack.cache_extras(scratch, |oid| validated.push(oid));
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
            && work.items.iter().any(|item| {
                item.demand
                    .needs_pack(item.oid, scratch, &work.transfer, work.speculative)
            });
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
                queue.pending.remove(&item.oid);
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
            if readers
                .iter()
                .any(|(oid, demand)| demand.needs_pack(*oid, &cache, &transfer, speculative))
            {
                queue.failure.get_or_insert_with(|| format!("{error:#}"));
            }
        })
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
                    .then(|| queue.batch_wait(Instant::now(), !active.is_empty()))
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
    ) -> Result<()> {
        if let Some(mut standby) = standby {
            standby.set_stop(self.stop.clone());
            if let Ok(Some(connected)) = standby.negotiate() {
                session = Some(connected);
                self.additional_starts.fetch_add(1, Ordering::Relaxed);
            }
            self.queue.lock().unwrap().starting[index] = false;
            self.work.notify_all();
        }
        loop {
            let next = {
                let mut queue = self.queue.lock().unwrap();
                let needs_ssh_channel = index != 0 && session.is_none() && connection.is_ssh();
                loop {
                    if self.stop.load(Ordering::Relaxed)
                        || queue.native_ceiling.is_some_and(|ceiling| index >= ceiling)
                    {
                        return session.as_ref().map_or(Ok(()), Session::stop);
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
            let (work, failure) = match next {
                NativeWork::Fetch(work, failure) => (work, failure),
                NativeWork::Connect => {
                    let connected = connection.connect_additional(self.stop.clone());
                    let mut queue = self.queue.lock().unwrap();
                    queue.busy[index] = false;
                    queue.connecting = false;
                    if let Ok(Some(connected)) = connected {
                        session = Some(connected);
                        self.additional_starts.fetch_add(1, Ordering::Relaxed);
                    } else if !self.stop.load(Ordering::Relaxed) {
                        // An additional session can exceed the master's
                        // MaxSessions. The queue still owns all the demands.
                        queue.native_ceiling =
                            Some(queue.native_ceiling.unwrap_or(CONNECTIONS).min(index));
                        queue.prune_prefetch();
                    }
                    self.work.notify_all();
                    continue;
                }
            };
            let started = Instant::now();
            if session.is_none() && failure.is_none() {
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
                    session
                        .as_mut()
                        .unwrap()
                        .fetch_stream(&oids, self.response(&work, &oids, scratch))
                })
            };
            if result.is_err() {
                // fetch_stream reaps a channel with an incomplete response.
                session = None;
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
        let Some(NativeWork::Fetch(work, None)) = queue.native_work(4, false) else {
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
            assert!(data.len() < 16);
            pack.push((*kind as u8) << 4 | data.len() as u8);
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
            FreshDecode,
            RetiredForecast,
        }
        for completion in [
            Completion::Extra,
            Completion::CachedDecode,
            Completion::FreshDecode,
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
            let reads_exact = matches!(completion, Completion::FreshDecode);
            if let Some((old, mut response, report)) = retired {
                assert!(!Arc::ptr_eq(&old.0, &demand.0));
                response
                    .write_http(&pack_reply(&[(Blob, data)], false))
                    .unwrap();
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
                let Some(crate::raw_blob::ReceivedObject::Verified(extra)) = scratch.received(oid)
                else {
                    panic!("validated pack did not publish its extra");
                };
                // Leave the extra unhydrated when the exact decoder is
                // supposed to supply the raw blob before the read.
                if !reads_exact {
                    scratch
                        .insert_verified_blob(extra, None, false, || false, |_| {})
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
            let expected =
                (reads_exact || matches!(completion, Completion::RetiredForecast)).then_some(RESET);
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
        pack.cache_extras(&scratch, |oid| validated.push(oid));
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
        scratch
            .insert_fetched(
                oid,
                data.len(),
                &data[..],
                Some(Arc::downgrade(&work.transfer)),
            )
            .unwrap();
        shared.complete_object(oid, &demand, true, Acquired::Stored);
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
                .insert_fetched(
                    oid,
                    data.len(),
                    &data[..],
                    Some(Arc::downgrade(&work.transfer)),
                )
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
            .request_with_lookup(oid, || {
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
    fn a_satisfied_forecast_leaves_the_foreground_decode_buffer_available() {
        let directory = tempfile::tempdir().unwrap();
        let cache = RawBlobs::new(directory.path()).unwrap();
        let held: Vec<_> = (0..7)
            .map(|_| {
                cache
                    .reserve_pack_decode(|| false, object::check_transfer)
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
        let Some(NativeWork::Fetch(work, None)) = queue.native_work(0, false) else {
            panic!("the ready channel did not receive the pending demand");
        };
        assert_eq!(work.wants(), [oid]);
        assert!(queue.waiting.is_empty());
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
    fn batch_waits_for_more_demands_until_target_or_deadline() {
        let now = Instant::now();
        let mut queue = Queue::default();
        queue.waiting.push_back(QueuedDemand {
            oid: Oid::zero(),
            demand: Demand::default(),
            queued_at: now,
            prefetch_width: READ_AHEAD,
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
