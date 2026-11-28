//! One request queue for HTTP/2 and owned SSH channels.
use crate::{
    object, pack_stream,
    raw_blob::{is_retryable_spill, RawBlobs},
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
        mpsc, Arc, Condvar, Mutex, Weak,
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
pub(crate) const HTTP2_FOREGROUND: usize = 32;
const HTTP2_READ_AHEAD: usize = 8;

/// Tracks response tails still needed after the command has used their data.
#[derive(Default)]
pub(crate) struct ResponseCompletion {
    state: Mutex<CompletionState>,
    changed: Condvar,
}

#[derive(Default)]
struct CompletionState {
    observed: Vec<ObservedResponse>,
    sealed: bool,
    failed: bool,
}

struct ObservedResponse {
    receipt: Arc<ResponseReceipt>,
    input_epoch: u64,
    waiting_since: Instant,
}

impl CompletionState {
    fn next_deadline(&mut self, now: Instant, grace: Duration) -> Option<Instant> {
        self.observed
            .iter_mut()
            .filter_map(|response| {
                let epoch = response.receipt.input_epoch.load(Ordering::Relaxed);
                if epoch != response.input_epoch {
                    response.input_epoch = epoch;
                    response.waiting_since = now;
                }
                (epoch & 1 != 0).then_some(response.waiting_since + grace)
            })
            .min()
    }
}

impl ResponseCompletion {
    pub(crate) fn wait(&self) -> Result<()> {
        self.wait_for(Duration::from_secs(30))
    }

    fn wait_for(&self, grace: Duration) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if !state.sealed {
            state.sealed = true;
            let started = Instant::now();
            for response in &mut state.observed {
                response.waiting_since = started;
            }
        }
        while !state.observed.is_empty()
            && !state.failed
            && crate::git::cancelled_signal().is_none()
        {
            let now = Instant::now();
            let poll = Duration::from_millis(20);
            let remaining = state
                .next_deadline(now, grace)
                .map_or(poll, |deadline| deadline.saturating_duration_since(now));
            anyhow::ensure!(
                !remaining.is_zero(),
                "timed out waiting for Git packs that supplied data to finish"
            );
            state = self
                .changed
                .wait_timeout(state, remaining.min(poll))
                .unwrap()
                .0;
        }
        Ok(())
    }
}

#[derive(Default)]
struct ReceiptState {
    waiting: Option<Weak<ResponseCompletion>>,
    finished: bool,
    failure: Option<String>,
}

#[derive(Default)]
pub(crate) struct ResponseReceipt {
    state: Mutex<ReceiptState>,
    // Odd during an input wait. A new wait advances the epoch even if the
    // observer never saw the intervening processing state.
    input_epoch: AtomicU64,
}

pub(crate) struct InputWait<'a>(&'a AtomicU64);

impl Drop for InputWait<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

impl ResponseReceipt {
    pub(crate) fn waiting_for_input(&self) -> InputWait<'_> {
        self.input_epoch.fetch_add(1, Ordering::Relaxed);
        InputWait(&self.input_epoch)
    }

    pub(crate) fn observe(self: &Arc<Self>, completion: &Arc<ResponseCompletion>) -> Result<bool> {
        let mut receipt = self.state.lock().unwrap();
        if let Some(error) = &receipt.failure {
            bail!("{error}");
        }
        if !receipt.finished && receipt.waiting.is_none() {
            let mut state = completion.state.lock().unwrap();
            if !state.sealed {
                state.observed.push(ObservedResponse {
                    receipt: self.clone(),
                    input_epoch: self.input_epoch.load(Ordering::Relaxed),
                    waiting_since: Instant::now(),
                });
                receipt.waiting = Some(Arc::downgrade(completion));
            }
        }
        Ok(!receipt.finished)
    }

    pub(crate) fn fail(&self, error: &anyhow::Error) -> bool {
        let mut receipt = self.state.lock().unwrap();
        receipt.failure = Some(format!("{error:#}"));
        if let Some(completion) = receipt.waiting.as_ref().and_then(Weak::upgrade) {
            // The queue retains this failure for the final exit status.
            completion.state.lock().unwrap().failed = true;
            completion.changed.notify_all();
        }
        receipt.waiting.is_some()
    }
}

#[derive(Default)]
struct ResponseGuard(Arc<ResponseReceipt>);

impl Drop for ResponseGuard {
    fn drop(&mut self) {
        let mut receipt = self.0.state.lock().unwrap();
        receipt.finished = true;
        if let Some(completion) = receipt.waiting.take().and_then(|waiting| waiting.upgrade()) {
            completion
                .state
                .lock()
                .unwrap()
                .observed
                .retain(|response| !Arc::ptr_eq(&response.receipt, &self.0));
            completion.changed.notify_all();
        }
    }
}

pub(crate) enum Acquired {
    Stored(Oid),
    Packed(pack_stream::PackedObject),
    Object(pack_stream::VerifiedObject),
}

pub(crate) struct Batch {
    pub(crate) oids: Vec<Oid>,
    delivery: Delivery,
    result: Mutex<BatchResult>,
}

#[derive(PartialEq, Eq)]
enum Delivery {
    Response,
    Cached,
    ValidatedExtra,
    Failed,
}

struct BatchResult {
    acquired: Option<Result<Acquired>>,
    failure: Option<Arc<anyhow::Error>>,
}

#[derive(Debug)]
struct PublicationError(Arc<anyhow::Error>);

impl std::fmt::Display for PublicationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("publish remote Git object")
    }
}

impl std::error::Error for PublicationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}

impl Batch {
    fn new(oids: Vec<Oid>, result: Result<Acquired>) -> Arc<Self> {
        Arc::new(Self {
            oids,
            delivery: match &result {
                Ok(Acquired::Packed(_)) => Delivery::ValidatedExtra,
                Ok(Acquired::Stored(_)) => Delivery::Cached,
                Ok(Acquired::Object(_)) => Delivery::Response,
                Err(_) => Delivery::Failed,
            },
            result: Mutex::new(BatchResult {
                acquired: Some(result),
                failure: None,
            }),
        })
    }

    pub(crate) fn publish(
        &self,
        publish: impl FnOnce(Result<Acquired>) -> Result<()>,
    ) -> Result<()> {
        let mut result = self.result.lock().unwrap();
        if let Some(acquired) = result.acquired.take() {
            result.failure = publish(acquired).err().map(Arc::new);
        }
        match &result.failure {
            Some(error) => Err(PublicationError(error.clone()).into()),
            None => Ok(()),
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
    response: Option<Arc<ResponseReceipt>>,
    covered_by_extra: bool,
    #[cfg(target_os = "macos")]
    notify: Option<Arc<tokio::sync::Notify>>,
}

impl Demand {
    fn complete(&self, batch: Arc<Batch>) {
        let mut state = self.0.state.lock().unwrap();
        // A checked extra also covers a flight that already has early bytes.
        state.covered_by_extra |= batch.delivery == Delivery::ValidatedExtra;
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

    pub(crate) fn wait(&self, completion: &Arc<ResponseCompletion>) -> Arc<Batch> {
        let state = self.0.state.lock().unwrap();
        let state = self
            .0
            .ready
            .wait_while(state, |state| state.batch.is_none())
            .unwrap();
        let batch = state.batch.as_ref().unwrap();
        if batch.delivery == Delivery::Response {
            state.observe(completion);
        }
        batch.clone()
    }

    pub(crate) fn ready(&self) -> bool {
        self.0.state.lock().unwrap().batch.is_some()
    }

    fn covered_by_extra(&self) -> bool {
        self.0.state.lock().unwrap().covered_by_extra
    }

    fn required(&self) -> bool {
        self.0.required.load(Ordering::Relaxed)
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

impl FlightState {
    fn observe(&self, completion: &Arc<ResponseCompletion>) {
        if let Some(response) = &self.response {
            let _ = response.observe(completion);
        }
    }
}

struct QueuedDemand {
    oid: Oid,
    demand: Demand,
    queued_at: Instant,
    prefetch_width: usize,
    standalone: bool,
}

struct Work {
    items: Vec<QueuedDemand>,
    speculative: bool,
    response: ResponseGuard,
}

impl Work {
    fn take(queue: &mut VecDeque<QueuedDemand>, speculative: bool) -> Option<Self> {
        let first = queue.front()?;
        let count = if first.standalone {
            1
        } else {
            let limit = if speculative {
                first.prefetch_width
            } else {
                MAX_WANTS
            };
            queue
                .iter()
                .take(limit)
                .take_while(|item| !item.standalone)
                .count()
        };
        let items: Vec<_> = queue.drain(..count).collect();
        let response = ResponseGuard::default();
        for item in &items {
            item.demand.0.state.lock().unwrap().response = Some(response.0.clone());
        }
        Some(Self {
            items,
            speculative,
            response,
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
    busy: [bool; CONNECTIONS],
    initial_channels: usize,
    starting: [bool; CONNECTIONS],
    connecting: bool,
    native_ceiling: Option<usize>,
    failure: Option<String>,
    mode: ConnectionMode,
    http2: bool,
}

impl Queue {
    fn can_dispatch_http2(&self, (foreground, ahead): (usize, usize)) -> bool {
        let failed = self.failure.is_some();
        (!self.waiting.is_empty() && (foreground > 0 || failed))
            || (!self.ahead.is_empty() && (ahead > 0 || failed))
    }

    fn insert(&mut self, oid: Oid) -> QueuedDemand {
        let demand = Demand::default();
        self.pending.insert(oid, demand.clone());
        QueuedDemand {
            oid,
            demand,
            queued_at: Instant::now(),
            prefetch_width: READ_AHEAD,
            standalone: false,
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
        match self.forecast_score {
            2..=3 => 64,
            4..=5 => 128,
            _ => MAX_READ_AHEAD,
        }
    }

    fn dense(&self) -> bool {
        self.forecast_score >= DENSE_SCORE && self.can_expand_forecast()
    }

    fn can_expand_forecast(&self) -> bool {
        // Additional HTTP/2 requests share the same connection. A confirmed
        // scan can benefit from larger batches even when the first replies
        // were too fast to justify opening more SSH channels.
        self.http2 || !matches!(self.mode, ConnectionMode::Standard)
    }

    fn prediction_room(&mut self, width: usize) -> usize {
        let capacity = (4 * width).min(MAX_UNCONSUMED_PREDICTIONS);
        if self.predicted.len() >= capacity && self.can_expand_forecast() {
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
        if !transfer_active || self.waiting.front().is_some_and(|item| item.standalone) {
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
            (first, DENSE_BATCH_WINDOW)
        } else if matches!(self.mode, ConnectionMode::Wide) && self.waiting.len() < BATCH_TARGET {
            (self.waiting.front()?.queued_at, BATCH_WINDOW)
        } else {
            return None;
        };
        window
            .checked_sub(now.duration_since(first))
            .filter(|remaining| !remaining.is_zero())
    }

    fn native_work(&mut self, index: usize, needs_ssh_channel: bool) -> Option<NativeWork> {
        let speculative = self.waiting.is_empty();
        let ready_for_exact = !speculative && !needs_ssh_channel;
        let channels = if self.dense() {
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
            self.remove_request(&item);
        }
    }

    fn remove_request(&mut self, item: &QueuedDemand) {
        if self
            .pending
            .get(&item.oid)
            .is_some_and(|current| Arc::ptr_eq(&current.0, &item.demand.0))
        {
            self.pending.remove(&item.oid);
        }
    }

    fn complete_extra(&mut self, oid: Oid, object: pack_stream::PackedObject) {
        if let Some(demand) = self.pending.get(&oid) {
            // A speculative request already in flight still holds this demand.
            // Mark coverage before removing its queue entry.
            demand.complete(Batch::new(vec![oid], Ok(Acquired::Packed(object))));
            if !demand.required() {
                self.pending.remove(&oid);
            }
            self.waiting.retain(|item| item.oid != oid);
            self.ahead.retain(|item| item.oid != oid);
        }
    }

    fn prune_prefetch(&mut self) {
        if self.native_ceiling == Some(1) {
            while let Some(item) = self.ahead.pop_front() {
                self.remove_request(&item);
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
    pub(crate) omitted_wants: u64,
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
    omitted_wants: AtomicU64,
}

struct Http2Batch {
    work: Work,
    started: Instant,
}

impl Http2Batch {
    fn free_slots(active: &HashMap<usize, Self>) -> (usize, usize) {
        let ahead = active
            .values()
            .filter(|batch| batch.work.speculative)
            .count();
        (
            HTTP2_FOREGROUND.saturating_sub(active.len() - ahead),
            HTTP2_READ_AHEAD.saturating_sub(ahead),
        )
    }
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
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                http2: true,
                ..Queue::default()
            }),
            ..Shared::default()
        });
        let worker_state = shared.clone();
        let worker_scratch = scratch.clone();
        let decoder_signal = signal.clone();
        let (ready, result) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("git-on-http2".into())
            .spawn(move || {
                let client = crate::http2::Client::negotiate(
                    configuration,
                    worker_state.stop.clone(),
                    branch.as_ref(),
                );
                match client {
                    Ok(Some((client, selected))) => {
                        if ready.send(Ok(Some(selected))).is_ok() {
                            worker_state.run_http2(client, read, &worker_scratch, &decoder_signal);
                        }
                    }
                    Ok(None) => {
                        let _ = ready.send(Ok(None));
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
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
        if !queue.pending.contains_key(&oid) {
            let ready = if let Some(object) = self.scratch.packed_object(oid) {
                Some(Acquired::Packed(object))
            } else {
                self.scratch
                    .get(oid)?
                    .is_some()
                    .then_some(Acquired::Stored(oid))
            };
            if let Some(ready) = ready {
                let demand = Demand::default();
                demand.complete(Batch::new(Vec::new(), Ok(ready)));
                return Ok(demand);
            }
        }
        let demand = queue.foreground(oid);
        self.wake();
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
        for oid in candidates(queue.dense()) {
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
        // Batch::publish calls this once for its required demands. Workers
        // retire unused speculative requests through Queue::remove_request.
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
            omitted_wants: s.omitted_wants.load(Ordering::Relaxed),
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
    fn response(
        self: &Arc<Self>,
        work: &Work,
        oids: &[Oid],
        scratch: &RawBlobs,
    ) -> pack_stream::Response {
        let shared = self.clone();
        let speculative = work.speculative;
        let tasks: Vec<_> = work
            .items
            .iter()
            .map(|item| (item.oid, item.demand.clone()))
            .collect();
        pack_stream::Response::new(
            oids.to_vec(),
            Box::new(move |object| {
                let oid = object.oid();
                let demand = &tasks
                    .iter()
                    .find(|(wanted, _)| *wanted == oid)
                    .expect("known want")
                    .1;
                let acquired = if object.kind() == git2::ObjectType::Blob {
                    if speculative {
                        shared.prefetched_blobs.fetch_add(1, Ordering::Relaxed);
                    }
                    Acquired::Stored(oid)
                } else {
                    anyhow::ensure!(!speculative, "prefetched object is not a blob");
                    Acquired::Object(object)
                };
                shared.oids.fetch_add(1, Ordering::Relaxed);
                demand.complete(Batch::new(vec![oid], Ok(acquired)));
                Ok(())
            }),
        )
        .cache_blobs(scratch.clone(), Some(work.response.0.clone()))
    }

    fn finish(
        &self,
        mut work: Work,
        result: Result<pack_stream::Received>,
        started: Instant,
        lane: Option<usize>,
        scratch: &RawBlobs,
    ) {
        let result = if lane.is_none() {
            result.with_context(|| format!("fetch exact Git object {}", work.items[0].oid))
        } else {
            result
        };
        let mut queue = self.queue.lock().unwrap();
        let result = result.map(|mut pack| {
            self.packs.fetch_add(1, Ordering::Relaxed);
            self.bytes.fetch_add(pack.bytes, Ordering::Relaxed);
            self.streamed_packs.fetch_add(1, Ordering::Relaxed);
            if lane.is_none() {
                self.http2_fetches.fetch_add(1, Ordering::Relaxed);
            }
            if work.speculative {
                self.prefetches.fetch_add(1, Ordering::Relaxed);
            }
            for (oid, object) in pack.objects {
                // Extras become available only after the whole response
                // passes validation. A duplicate request can finish from it
                // even if its own response is later reset.
                scratch.remember_packed(object.clone());
                queue.complete_extra(oid, object);
            }
            for item in &work.items {
                if let Some(error) = pack.cache_errors.remove(&item.oid) {
                    item.demand.complete(Batch::new(vec![item.oid], Err(error)));
                }
            }
            pack.incomplete
        });

        if let Some(index) = lane {
            queue.busy[index] = false;
        }
        // A speculative pack can already have exposed verified bytes before
        // its trailer or transport fails. Retrying an outstanding want must
        // not erase that failure.
        let failed_used_channel = result.as_ref().is_err_and(|error| {
            error.is::<crate::transport::UsedSshChannelFailure>()
                && crate::git::cancelled_signal().is_none()
        });
        // The exact request can already have been satisfied by an extra while
        // this response supplied the bytes actually admitted to the raw cache.
        let failed_after_read = result.as_ref().err().is_some_and(|error| {
            !object::is_cancelled(error)
                && !is_retryable_spill(error)
                && scratch.reject_response(
                    &work.response.0,
                    work.items
                        .iter()
                        .filter(|item| item.demand.covered_by_extra())
                        .map(|item| item.oid),
                    error,
                )
        });
        let needed = failed_after_read
            || result.is_err()
                && work.items.iter().any(|item| {
                    !item.demand.covered_by_extra() && (!work.speculative || item.demand.ready())
                });
        // Retire a speculative success from the queue once its verified blob
        // is visible in the raw store. A foreground waiter still holds a clone.
        work.items.retain(|item| {
            if !item.demand.ready() {
                return true;
            }
            if work.speculative && !item.demand.required() {
                queue.remove_request(item);
            }
            false
        });
        if matches!(result, Ok(true)) {
            // GitHub can omit a blob from a filtered mixed tree/blob reply.
            // Ready objects stay published, and each outstanding exact want
            // gets one isolated request. A single missing want is an error.
            self.omitted_wants
                .fetch_add(work.items.len() as u64, Ordering::Relaxed);
            for mut item in work.items.drain(..) {
                item.standalone = true;
                queue.retry(item, work.speculative);
            }
        }
        let result = match result {
            Ok(_) => {
                queue.mode.record_reply(started.elapsed());
                None
            }
            Err(error)
                if work.speculative
                    && !failed_used_channel
                    && !needed
                    && !self.stop.load(Ordering::Relaxed)
                    && queue.failure.is_none()
                    && !object::is_cancelled(&error)
                    && !is_retryable_spill(&error) =>
            {
                for item in work.items {
                    queue.retry(item, true);
                }
                self.work.notify_all();
                return;
            }
            Err(error) => {
                if work.speculative {
                    work.items.retain(|item| {
                        if item.demand.required() {
                            true
                        } else {
                            queue.remove_request(item);
                            false
                        }
                    });
                }
                if failed_used_channel
                    || needed && !object::is_cancelled(&error) && !is_retryable_spill(&error)
                {
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

    fn run_http2(
        self: &Arc<Self>,
        mut client: crate::http2::Client,
        mut wake: UnixStream,
        scratch: &RawBlobs,
        decoder_signal: &Arc<UnixStream>,
    ) {
        let outcome = self.drive_http2(&mut client, &mut wake, scratch, decoder_signal);
        self.finish_http2(outcome.err().filter(|error| !object::is_cancelled(error)));
    }

    fn finish_http2(&self, failure: Option<anyhow::Error>) {
        let pending = {
            let mut queue = self.queue.lock().unwrap();
            if let Some(error) = &failure {
                queue.failure.get_or_insert_with(|| format!("{error:#}"));
            } else {
                // Close admission before draining. A request may already have
                // passed the process-wide cancellation check.
                self.stop.store(true, Ordering::Relaxed);
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
        scratch: &RawBlobs,
        decoder_signal: &Arc<UnixStream>,
    ) -> Result<()> {
        let mut active = HashMap::<usize, Http2Batch>::new();
        let mut token = 0_usize;
        while !self.stop.load(Ordering::Relaxed) {
            object::check_transfer()?;
            drain_wake(wake)?;
            let (waiting, ahead, failure) = {
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
                (waiting, ahead, queue.failure.clone())
            };
            let (foreground_slots, prefetch_slots) = Http2Batch::free_slots(&active);
            for (mut batches, speculative, mut slots) in [
                (waiting, false, foreground_slots),
                (ahead, true, prefetch_slots),
            ] {
                while !batches.is_empty() {
                    if failure.is_none() && slots == 0 {
                        let mut queue = self.queue.lock().unwrap();
                        if speculative {
                            batches.append(&mut queue.ahead);
                            for item in batches {
                                if item.demand.required() {
                                    queue.waiting.push_back(item);
                                } else {
                                    queue.ahead.push_back(item);
                                }
                            }
                        } else {
                            batches.append(&mut queue.waiting);
                            queue.waiting = batches;
                        }
                        break;
                    }
                    let work = Work::take(&mut batches, speculative).unwrap();
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
                                decoder_signal,
                            )
                        }
                    };
                    match submitted {
                        Ok(()) => {
                            slots -= 1;
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
            let timeout = {
                let queue = self.queue.lock().unwrap();
                queue
                    .failure
                    .is_none()
                    .then(|| queue.batch_wait(Instant::now(), !active.is_empty()))
                    .flatten()
                    .map(|remaining| remaining.max(Duration::from_millis(1)))
                    .or_else(|| {
                        (!queue.can_dispatch_http2(Http2Batch::free_slots(&active)))
                            .then_some(Duration::from_secs(1))
                    })
            };
            if let Some(timeout) = timeout {
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
                let needs_ssh_channel = index != 0 && session.is_none();
                loop {
                    if self.stop.load(Ordering::Relaxed)
                        || queue.native_ceiling.is_some_and(|ceiling| index >= ceiling)
                    {
                        break None;
                    }
                    if let Some(delay) =
                        queue.batch_wait(Instant::now(), queue.busy.contains(&true))
                    {
                        queue = self.work.wait_timeout(queue, delay).unwrap().0;
                        continue;
                    }
                    if let Some(work) = queue.native_work(index, needs_ssh_channel) {
                        break Some(work);
                    }
                    queue = self.work.wait(queue).unwrap();
                }
            };
            self.work.notify_all();
            let Some(next) = next else {
                if let Some(mut session) = session {
                    if let Some(failure) = session.stop()? {
                        self.queue.lock().unwrap().failure.get_or_insert(failure);
                    }
                }
                return Ok(());
            };
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
                let session = session.as_mut().unwrap();
                session
                    .check_connection(&connection)
                    .and_then(|_| session.fetch_stream(&oids, self.response(&work, &oids, scratch)))
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
    fn completed_command_times_out_while_a_used_pack_is_still_open() {
        let grace = Duration::from_millis(40);
        let completion = Arc::new(ResponseCompletion::default());
        let response = ResponseGuard::default();
        let reading = response.0.waiting_for_input();
        assert!(response.0.observe(&completion).unwrap());

        let (send, receive) = mpsc::channel();
        let waiting = completion.clone();
        let waiter = thread::spawn(move || {
            let start = Instant::now();
            let result = waiting.wait_for(grace);
            let _ = send.send((result, start.elapsed()));
        });
        let (outcome, elapsed) = receive
            .recv_timeout(Duration::from_secs(3))
            .expect("an open response stalled the wait");
        waiter.join().unwrap();
        assert!(elapsed >= grace);
        assert!(outcome
            .unwrap_err()
            .to_string()
            .contains("packs that supplied data"));
        drop(reading);
        drop(response);
        assert!(completion.wait_for(Duration::ZERO).is_ok());
    }

    #[test]
    fn input_deadlines_are_independent_and_pause_while_processing() {
        let completion = Arc::default();
        let active = ResponseGuard::default();
        let stalled = ResponseGuard::default();
        let reading = active.0.waiting_for_input();
        let stalled_reading = stalled.0.waiting_for_input();
        assert!(active.0.observe(&completion).unwrap());
        assert!(stalled.0.observe(&completion).unwrap());
        let grace = Duration::from_secs(30);
        let start = Instant::now();
        let received = start + Duration::from_secs(29);
        for response in &mut completion.state.lock().unwrap().observed {
            response.waiting_since = start;
        }
        drop(reading);
        let reading = active.0.waiting_for_input();
        let earliest = completion
            .state
            .lock()
            .unwrap()
            .next_deadline(received, grace);
        assert_eq!(earliest, Some(start + grace));
        drop(stalled_reading);
        drop(stalled);
        let deadline = completion
            .state
            .lock()
            .unwrap()
            .next_deadline(start + grace, grace);
        assert_eq!(deadline, Some(received + grace));
        drop(reading);
        let while_processing = completion
            .state
            .lock()
            .unwrap()
            .next_deadline(start + grace * 3, grace);
        assert_eq!(while_processing, None);
    }

    #[test]
    fn http2_cancellation_drains_and_closes_the_demand_queue() {
        let directory = tempfile::tempdir().unwrap();
        let pool = Pool {
            shared: Arc::default(),
            scratch: RawBlobs::new(directory.path()).unwrap(),
            workers: Mutex::default(),
            wake: None,
        };
        let oid = Oid::zero();
        let pending = pool.request(oid).unwrap();

        // Model a caller that already passed the process-wide transfer check.
        pool.shared.finish_http2(None);
        assert!(pending.ready());
        let error = pool.request(oid).err().expect("dispatcher is already gone");
        assert!(object::is_cancelled(&error));
    }

    #[test]
    fn http2_dispatch_refills_a_freed_read_ahead_slot_without_foreground_work() {
        let mut queue = Queue::default();
        let queued = queue.insert(Oid::zero());
        queue.ahead.push_back(queued);
        assert!(!queue.can_dispatch_http2((HTTP2_FOREGROUND, 0)));
        assert!(queue.can_dispatch_http2((HTTP2_FOREGROUND, 1)));
    }

    #[test]
    fn joined_waiters_share_a_retryable_publication_error() {
        let pending = Demand::default();
        let joined = pending.clone();
        let completion = Arc::default();
        pending.complete(Batch::new(Vec::new(), Ok(Acquired::Stored(Oid::zero()))));
        let error = pending
            .wait(&completion)
            .publish(|_| {
                Err(
                    crate::raw_blob::BackingError(io::Error::from_raw_os_error(libc::ENOSPC))
                        .into(),
                )
            })
            .unwrap_err();
        assert!(is_retryable_spill(&error));
        let same_failure = joined
            .wait(&completion)
            .publish(|_| panic!("an acquired object is published only once"))
            .unwrap_err();
        assert!(is_retryable_spill(&same_failure));
    }

    #[test]
    fn late_reader_of_a_prefetched_object_waits_for_its_response() {
        let (_, oid, prefix, _, _) = two_blob_response();
        let directory = tempfile::tempdir().unwrap();
        let cache = RawBlobs::new(directory.path()).unwrap();
        let shared = Arc::new(Shared::default());
        let mut queue = Queue::default();
        let item = queue.insert(oid);
        let demand = item.demand.clone();
        queue.ahead.push_back(item);
        let work = Work::take(&mut queue.ahead, true).unwrap();
        let completion = Arc::default();

        assert!(!demand.required());
        shared
            .response(&work, &[oid], &cache)
            .write_http(&prefix)
            .unwrap();
        let batch = queue.foreground(oid).wait(&completion);
        batch
            .publish(|payload| {
                let Acquired::Stored(oid) = payload? else {
                    panic!("missing stored blob");
                };
                assert!(cache.observe_cached(oid, &completion)?);
                Ok(())
            })
            .unwrap();
        for oid in &batch.oids {
            queue.pending.remove(oid);
        }
        assert_eq!(completion.state.lock().unwrap().observed.len(), 1);
        drop(work);
        assert!(completion.state.lock().unwrap().observed.is_empty());
    }

    fn two_blob_response() -> (Oid, Oid, Vec<u8>, usize, Vec<u8>) {
        use git2::ObjectType;
        use sha1_checked::{Digest, Sha1};

        let a = Oid::hash_object(ObjectType::Blob, b"a").unwrap();
        let b = Oid::hash_object(ObjectType::Blob, b"b").unwrap();
        let mut objects = b"PACK\0\0\0\x02\0\0\0\x02".to_vec();
        let mut split = 0;
        for contents in [b"a", b"b"] {
            objects.push(0x31);
            let mut encoder = flate2::write::ZlibEncoder::new(&mut objects, Default::default());
            encoder.write_all(contents).unwrap();
            encoder.finish().unwrap();
            if contents == b"a" {
                split = objects.len();
            }
        }
        let mut prefix = format!("000dpackfile\n{:04x}\x01", objects.len() + 25).into_bytes();
        split += prefix.len();
        prefix.extend_from_slice(&objects);
        let mut tail = Sha1::digest(&objects).to_vec();
        tail.extend_from_slice(b"0000");
        (a, b, prefix, split, tail)
    }

    #[test]
    fn reused_ssh_channel_retains_an_exit_during_unused_prefetch() {
        use std::{fs, os::unix::fs::MetadataExt, os::unix::net::UnixListener, process::Command};

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("control");
        let _listener = UnixListener::bind(&socket).unwrap();
        let metadata = fs::symlink_metadata(&socket).unwrap();
        let connection = Connection::new(
            Command::new("false"),
            crate::transport::SshSocket {
                path: socket,
                device: metadata.dev(),
                inode: metadata.ino(),
            },
        );
        let (wanted, _, prefix, _, tail) = two_blob_response();
        let response = directory.path().join("response");
        fs::write(&response, [prefix, tail].concat()).unwrap();
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-ec",
                r#"printf '000eversion 2\n0019fetch=shallow filter\n0000'
dd bs=1 count="$1" of=/dev/null 2>/dev/null
cat "$2"
dd bs=1 count="$1" of=/dev/null 2>/dev/null
exec 1>&-
sleep 0.2
exit 23"#,
                "fixture",
            ])
            .arg(
                crate::transport::exact_request(&[wanted])
                    .unwrap()
                    .len()
                    .to_string(),
            )
            .arg(response);
        let session = StartingSession::spawn_foreground(command)
            .unwrap()
            .negotiate()
            .unwrap()
            .unwrap();
        let mut pool = Pool {
            shared: Arc::default(),
            scratch: RawBlobs::new(directory.path()).unwrap(),
            workers: Mutex::default(),
            wake: None,
        };
        pool.attach_primary(session, connection).unwrap();
        pool.request(wanted)
            .unwrap()
            .wait(&Arc::default())
            .publish(|object| {
                assert!(matches!(object?, Acquired::Stored(_)));
                Ok(())
            })
            .unwrap();

        let never_read = Oid::from_bytes(&[0xaa; 20]).unwrap();
        pool.prefetch(wanted, |_| std::iter::once(never_read), |_| false);
        let mut queue = pool.shared.queue.lock().unwrap();
        assert!(queue.predicted.contains_key(&never_read));
        let deadline = Instant::now() + Duration::from_secs(5);
        while queue.pending.contains_key(&never_read) {
            assert!(Instant::now() < deadline, "unused prefetch did not finish");
            queue = pool
                .shared
                .work
                .wait_timeout(queue, deadline.saturating_duration_since(Instant::now()))
                .unwrap()
                .0;
        }
        drop(queue);
        pool.stop().unwrap();
        let failure = pool.failure().expect("the used SSH channel lost its exit");
        assert!(
            failure.contains("OpenSSH upload-pack failed with exit status: 23"),
            "{failure}"
        );
    }

    #[test]
    fn used_pack_is_checked_after_another_objects_spill_fails() {
        let (a, b, prefix, split, tail) = two_blob_response();
        for (error, valid) in [(libc::ENOSPC, false), (libc::EROFS, true)] {
            let directory = tempfile::tempdir().unwrap();
            let cache = RawBlobs::disk_only(directory.path()).unwrap();
            let shared = Arc::new(Shared::default());
            let completion = Arc::default();
            let (work, first, second) = {
                let mut queue = shared.queue.lock().unwrap();
                let first = queue.foreground(a);
                let second = queue.foreground(b);
                let work = Work::take(&mut queue.waiting, false).unwrap();
                (work, first, second)
            };
            let mut receiver = shared.response(&work, &[a, b], &cache);
            receiver.write_http(&prefix[..split]).unwrap();
            first
                .wait(&completion)
                .publish(|result| {
                    let Acquired::Stored(oid) = result? else {
                        panic!("missing stored blob");
                    };
                    assert!(cache.observe_cached(oid, &completion)?);
                    Ok(())
                })
                .unwrap();
            assert_eq!(completion.state.lock().unwrap().observed.len(), 1);
            cache.set_spill_error_for_test(Some(error));
            receiver.write_http(&prefix[split..]).unwrap();
            assert!(!second.ready());
            let mut checksum = tail.clone();
            if !valid {
                checksum[0] ^= 1;
            }
            receiver.write_http(&checksum).unwrap();
            shared.finish(work, receiver.finish_http(), Instant::now(), None, &cache);
            assert_eq!(shared.queue.lock().unwrap().failure.is_none(), valid);
            assert!(completion.state.lock().unwrap().observed.is_empty());
            assert_eq!(cache.get(a).unwrap().unwrap().read_all().unwrap(), b"a");

            if valid {
                let failure = second
                    .wait(&completion)
                    .publish(|result| result.map(|_| ()))
                    .unwrap_err();
                assert!(is_retryable_spill(&failure));
                cache.set_spill_error_for_test(None);
                let retry = {
                    let mut queue = shared.queue.lock().unwrap();
                    queue.pending.remove(&b);
                    queue.foreground(b);
                    Work::take(&mut queue.waiting, false).unwrap()
                };
                let mut receiver = shared.response(&retry, &[b], &cache);
                receiver.write_http(&prefix).unwrap();
                receiver.write_http(&tail).unwrap();
                shared.finish(retry, receiver.finish_http(), Instant::now(), None, &cache);
                assert_eq!(cache.get(b).unwrap().unwrap().read_all().unwrap(), b"b");
                assert!(shared.queue.lock().unwrap().failure.is_none());
            }
        }
    }

    #[test]
    fn only_a_validated_extra_can_satisfy_another_request() {
        let (a, b, prefix, _, checksum) = two_blob_response();
        for (valid, speculative, duplicate_first) in [
            (false, false, false),
            (true, false, false),
            (true, false, true),
            (true, true, false),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let cache = if speculative {
                RawBlobs::disk_only(directory.path()).unwrap()
            } else {
                RawBlobs::new(directory.path()).unwrap()
            };
            let shared = Arc::new(Shared::default());
            let (first, duplicate, waiting) = {
                let mut queue = shared.queue.lock().unwrap();
                queue.foreground(a);
                let first = Work::take(&mut queue.waiting, false).unwrap();
                let (waiting, duplicate) = if speculative {
                    let item = queue.insert(b);
                    let waiting = item.demand.clone();
                    queue.ahead.push_back(item);
                    (waiting, Work::take(&mut queue.ahead, true).unwrap())
                } else {
                    let waiting = queue.foreground(b);
                    (waiting, Work::take(&mut queue.waiting, false).unwrap())
                };
                (first, duplicate, waiting)
            };
            let mut receiver = shared.response(&first, &[a], &cache);
            receiver.write_http(&prefix).unwrap();
            assert!(cache.available(a));
            assert!(!cache.available(b));
            assert!(!waiting.ready());
            let mut duplicate_response = shared.response(&duplicate, &[b], &cache);
            if duplicate_first {
                duplicate_response.write_http(&prefix).unwrap();
                assert!(waiting.ready());
            }
            let mut tail = checksum.clone();
            if !valid {
                tail[0] ^= 1;
            }
            receiver.write_http(&tail).unwrap();
            shared.finish(first, receiver.finish_http(), Instant::now(), None, &cache);
            if valid {
                assert_eq!(cache.available(b), !speculative);
                let mut replacement = None;
                if speculative {
                    // The extra did not fit in the packed blob cache. A
                    // reader needs its own in-flight request for this OID.
                    let mut queue = shared.queue.lock().unwrap();
                    let requested = queue.foreground(b);
                    let work = Work::take(&mut queue.waiting, false).unwrap();
                    replacement = Some((requested, work));
                }
                if !duplicate_first {
                    duplicate_response.write_http(&prefix).unwrap();
                }
                drop(duplicate_response);
                assert!(cache.get(b).unwrap().is_some());
                shared.finish(
                    duplicate,
                    Err(anyhow::anyhow!("transfer reset")),
                    Instant::now(),
                    None,
                    &cache,
                );
                let mut queue = shared.queue.lock().unwrap();
                assert!(queue.failure.is_none());
                if let Some((replacement, _work)) = replacement {
                    assert!(Arc::ptr_eq(&queue.foreground(b).0, &replacement.0));
                }
                drop(queue);
                let completion = Arc::default();
                assert!(cache.observe_cached(b, &completion).unwrap());
                assert!(waiting.covered_by_extra());
                waiting
                    .wait(&completion)
                    .publish(|payload| {
                        match payload? {
                            Acquired::Packed(payload) => {
                                cache.insert_verified_blob(&payload.decode()?, None)?;
                            }
                            Acquired::Stored(oid) => assert_eq!(oid, b),
                            Acquired::Object(_) => panic!("missing blob"),
                        }
                        assert_eq!(cache.get(b).unwrap().unwrap().read_all()?, b"b");
                        Ok(())
                    })
                    .unwrap();
            } else {
                assert!(!cache.available(b));
                assert!(!waiting.ready());
                assert!(shared.queue.lock().unwrap().failure.is_some());
            }
        }
    }

    #[test]
    fn checked_extra_does_not_hide_a_transfer_that_supplied_the_read() {
        let (a, b, prefix, _, checksum) = two_blob_response();
        let directory = tempfile::tempdir().unwrap();
        let cache = RawBlobs::new(directory.path()).unwrap();
        let shared = Arc::new(Shared::default());
        let (first, duplicate, waiting) = {
            let mut queue = shared.queue.lock().unwrap();
            queue.foreground(a);
            let first = Work::take(&mut queue.waiting, false).unwrap();
            let waiting = queue.foreground(b);
            let duplicate = Work::take(&mut queue.waiting, false).unwrap();
            (first, duplicate, waiting)
        };
        let mut complete = shared.response(&first, &[a], &cache);
        complete.write_http(&prefix).unwrap();
        complete.write_http(&checksum).unwrap();
        shared.finish(first, complete.finish_http(), Instant::now(), None, &cache);
        assert!(waiting.covered_by_extra());
        assert!(cache.packed_object(b).is_some());
        assert!(cache.get(b).unwrap().is_none());

        let mut partial = shared.response(&duplicate, &[b], &cache);
        partial.write_http(&prefix).unwrap();
        let completion = Arc::default();
        assert!(cache.observe_cached(b, &completion).unwrap());
        let stalled = ResponseGuard::default();
        let _reading = stalled.0.waiting_for_input();
        assert!(stalled.0.observe(&completion).unwrap());
        assert_eq!(cache.get(b).unwrap().unwrap().read_all().unwrap(), b"b");
        shared.finish(
            duplicate,
            Err(anyhow::anyhow!("transfer that supplied the read reset")),
            Instant::now(),
            None,
            &cache,
        );
        assert!(shared.queue.lock().unwrap().failure.is_some());
        assert!(cache.observe_cached(b, &Arc::default()).is_err());
        assert!(completion.wait_for(Duration::ZERO).is_ok());
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
        let mut queue = Queue::default();
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

        queue.mode = ConnectionMode::Standard;
        for index in 0..6 {
            let oid = Oid::from_bytes(&[index; 20]).unwrap();
            queue.predicted.insert(oid, queue.forecast_step);
            assert_eq!(queue.forecast_width(oid), READ_AHEAD);
        }
        queue.http2 = true;
        assert_eq!(queue.forecast_width(Oid::zero()), MAX_READ_AHEAD);
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
                pending.complete(Batch::new(vec![other], Ok(Acquired::Stored(other))));
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
            standalone: false,
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
                standalone: false,
            });
        }
        assert_eq!(queue.batch_wait(now, true), None);
    }
}
