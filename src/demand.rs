//! One request queue for HTTP/2, native HTTPS helpers and owned SSH channels.
use crate::{
    git::Git,
    object, pack_stream,
    raw_blob::RawBlobs,
    spool::Spool,
    transport::{Connection, Session},
};
use anyhow::{bail, Context, Result};
use git2::Oid;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const CONNECTIONS: usize = 8;
const INITIAL_CONNECTIONS: usize = 4;
const MAX_WANTS: usize = 8;
const READ_AHEAD: usize = 4;
const MAX_READ_AHEAD: usize = 64;
const MAX_FORECAST_SCORE: u8 = 24;
const NATIVE_BLOB_STREAM_THRESHOLD: usize = 8 * 1024 * 1024;
const REPLY_SAMPLES: u8 = 8;
const SLOW_REPLY: Duration = Duration::from_millis(64);
const BATCH_TARGET: usize = 4;
const BATCH_WINDOW: Duration = Duration::from_millis(2);

pub(crate) enum Acquired {
    Stored,
    Object(pack_stream::VerifiedObject),
    Pack(Spool),
}

pub(crate) struct Batch {
    pub(crate) oids: Vec<Oid>,
    result: Mutex<Option<Result<Acquired>>>,
}

impl Batch {
    fn new(oids: Vec<Oid>, result: Result<Acquired>) -> Arc<Self> {
        Arc::new(Self {
            oids,
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
    #[cfg(target_os = "macos")]
    notify: Option<Arc<tokio::sync::Notify>>,
}

impl Demand {
    fn complete(&self, batch: Arc<Batch>) {
        let mut state = self.0.state.lock().unwrap();
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
    speculative: bool,
}

impl Work {
    fn take(queue: &mut VecDeque<QueuedDemand>, speculative: bool) -> Option<Self> {
        let limit = if speculative {
            queue.front()?.prefetch_width
        } else {
            MAX_WANTS
        };
        let count = queue.len().min(limit);
        (count != 0).then(|| Self {
            items: queue.drain(..count).collect(),
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
    predicted: HashSet<Oid>,
    forecast_score: u8,
    busy: [bool; CONNECTIONS],
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
        if self.predicted.remove(&anchor) {
            self.forecast_score = (self.forecast_score + 1).min(MAX_FORECAST_SCORE);
        } else {
            self.forecast_score = self.forecast_score.saturating_sub(2);
        }
        match self.forecast_score {
            0..=2 => READ_AHEAD,
            3..=5 => 16,
            6..=8 => 32,
            _ => MAX_READ_AHEAD,
        }
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
        if matches!(self.mode, ConnectionMode::Wide)
            && self.waiting.len() < BATCH_TARGET
            && transfer_active
        {
            return BATCH_WINDOW
                .checked_sub(now.duration_since(self.waiting.front()?.queued_at))
                .filter(|remaining| !remaining.is_zero());
        }
        None
    }

    fn native_work(&mut self, index: usize) -> Option<Work> {
        let channels = self
            .mode
            .channels()
            .min(self.native_ceiling.unwrap_or(CONNECTIONS));
        if index >= channels || self.busy[..index].contains(&false) {
            return None;
        }
        let active = self.busy.iter().filter(|busy| **busy).count();
        Work::take(&mut self.waiting, false).or_else(|| {
            (active + 1 < channels)
                .then(|| Work::take(&mut self.ahead, true))
                .flatten()
        })
    }

    fn retry(&mut self, mut item: QueuedDemand, speculative: bool) {
        if !speculative || item.demand.required() {
            item.queued_at = Instant::now();
            self.waiting.push_back(item);
        } else {
            self.pending.remove(&item.oid);
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
}

struct Http2Batch {
    work: Work,
    started: Instant,
}

pub(crate) struct Pool {
    shared: Arc<Shared>,
    scratch: RawBlobs,
    git: Arc<Git>,
    workers: Mutex<Vec<JoinHandle<Result<()>>>>,
    wake: Option<UnixStream>,
}

impl Pool {
    pub(crate) fn new(
        mut first: Session,
        connection: Connection,
        scratch: &RawBlobs,
        git: &Git,
    ) -> Result<Self> {
        let mut pool = Self {
            shared: Arc::default(),
            scratch: scratch.clone(),
            git: Arc::new(git.clone()),
            workers: Mutex::default(),
            wake: None,
        };
        first.set_stop(pool.shared.stop.clone());
        pool.start(0, Some(first), connection.clone())?;
        for index in 1..CONNECTIONS {
            pool.start(index, None, connection.clone())?;
        }
        Ok(pool)
    }

    pub(crate) fn http2(
        configuration: crate::http2::Configuration,
        scratch: &RawBlobs,
        git: &Git,
    ) -> Result<Option<Self>> {
        let (read, write) = UnixStream::pair()?;
        read.set_nonblocking(true)?;
        write.set_nonblocking(true)?;
        let shared = Arc::<Shared>::default();
        let worker_state = shared.clone();
        let worker_scratch = scratch.clone();
        let git = Arc::new(git.clone());
        let worker_git = git.clone();
        let (ready, result) = std::sync::mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("git-on-http2".into())
            .spawn(move || {
                let client =
                    crate::http2::Client::negotiate(configuration, worker_state.stop.clone());
                let Ok(Some(client)) = client else {
                    // Discovery sends no wants, so using native Git is safe here.
                    let _ = ready.send(false);
                    return Ok(());
                };
                if ready.send(true).is_ok() {
                    worker_state.run_http2(client, read, &worker_scratch, &worker_git);
                }
                Ok(())
            })?;
        let pool = Self {
            shared,
            scratch: scratch.clone(),
            git,
            workers: Mutex::new(vec![worker]),
            wake: Some(write),
        };
        let ready = result
            .recv()
            .context("Git HTTP/2 dispatcher stopped during discovery")?;
        object::check_transfer()?;
        Ok(ready.then_some(pool))
    }

    fn wake(&self) {
        self.shared.work.notify_all();
        if let Some(mut wake) = self.wake.as_ref() {
            // A full signal socket is already readable.
            let _ = wake.write(&[1]);
        }
    }

    fn start(
        &mut self,
        index: usize,
        session: Option<Session>,
        connection: Connection,
    ) -> Result<()> {
        let shared = self.shared.clone();
        let scratch = self.scratch.clone();
        let git = self.git.clone();
        let worker = thread::Builder::new()
            .name(format!("git-on-native-{index}"))
            .spawn(move || shared.worker(index, session, connection, &scratch, &git))?;
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
        if !queue.pending.contains_key(&oid) && self.scratch.available(oid) {
            let demand = Demand::default();
            demand.complete(Batch::new(vec![oid], Ok(Acquired::Stored)));
            return Ok(demand);
        }
        let demand = queue.foreground(oid);
        self.wake();
        Ok(demand)
    }

    pub(crate) fn prefetch(&self, anchor: Oid, candidates: impl IntoIterator<Item = Oid>) {
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
        let limit = width.min((4 * width).saturating_sub(queue.predicted.len()));
        if limit == 0 {
            return;
        }
        let mut accepted = 0;
        for oid in candidates {
            if queue.pending.contains_key(&oid)
                || queue.predicted.contains(&oid)
                || self.scratch.available(oid)
            {
                continue;
            }
            queue.predicted.insert(oid);
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
        let mut cache = scratch.clone();
        let speculative = work.speculative;
        let tasks: Vec<_> = work
            .items
            .iter()
            .map(|item| (item.oid, item.demand.clone()))
            .collect();
        pack_stream::Response::new(
            scratch.clone(),
            oids.to_vec(),
            Box::new(move |object| {
                let oid = object.oid();
                let demand = &tasks
                    .iter()
                    .find(|(wanted, _)| *wanted == oid)
                    .expect("known want")
                    .1;
                let acquired = if object.kind() == git2::ObjectType::Blob {
                    cache.insert_verified_blob(&object)?;
                    Acquired::Stored
                } else {
                    anyhow::ensure!(!speculative, "prefetched object is not a blob");
                    Acquired::Object(object)
                };
                shared.oids.fetch_add(1, Ordering::Relaxed);
                demand.complete(Batch::new(vec![oid], Ok(acquired)));
                Ok(())
            }),
        )
    }

    fn resolve_pack_remainder(
        &self,
        work: &Work,
        scratch: &RawBlobs,
        git: &Git,
        pack: Spool,
    ) -> Result<()> {
        let directory = Arc::new(scratch.index_directory()?);
        let mut options = git2::RepositoryInitOptions::new();
        options.bare(true).external_template(false);
        let repository = git2::Repository::init_opts(directory.path(), &options)?;
        let odb = repository.odb()?;
        pack_stream::index(&odb, &pack)?;
        // Keep unrequested bases compressed until they are needed. A
        // later demand can then import them without another network fetch.
        let mut cancelled = None;
        let indexed = odb.foreach(|oid| {
            if let Err(error) = object::check_transfer() {
                cancelled = Some(error);
                return false;
            }
            if !work.items.iter().any(|item| item.oid == *oid) {
                scratch.remember_packed(*oid, &directory);
            }
            true
        });
        if let Some(cancelled) = cancelled {
            return Err(cancelled);
        }
        indexed?;
        let mut cache = scratch.clone();
        for item in work.items.iter().filter(|item| !item.demand.ready()) {
            let (size, kind) = odb.read_header(item.oid)?;
            anyhow::ensure!(
                kind == git2::ObjectType::Blob,
                "prefetched object is not a blob"
            );
            if size > NATIVE_BLOB_STREAM_THRESHOLD {
                let mut command = git.command(Some(repository.path()));
                command
                    .args(["-c", "core.bigFileThreshold=8m", "cat-file", "blob"])
                    .arg(item.oid.to_string())
                    .stdin(std::process::Stdio::null());
                let (output, ()) = crate::git::run_with_output(command, None, |input| {
                    cache.insert_fetched(item.oid, size, input)?;
                    Ok(())
                })
                .context("stream large prefetched Git blob")?;
                anyhow::ensure!(
                    output.status.success(),
                    "cannot decode prefetched Git blob: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            } else {
                let object = odb.read(item.oid)?;
                cache.insert_prefetched_odb_blob(item.oid, &object)?;
            }
            self.oids.fetch_add(1, Ordering::Relaxed);
            item.demand
                .complete(Batch::new(vec![item.oid], Ok(Acquired::Stored)));
        }
        Ok(())
    }

    fn finish(
        &self,
        mut work: Work,
        result: Result<pack_stream::Received>,
        started: Instant,
        lane: Option<usize>,
        scratch: &RawBlobs,
        git: &Git,
    ) {
        let result = result.and_then(|pack| {
            self.packs.fetch_add(1, Ordering::Relaxed);
            self.bytes.fetch_add(pack.bytes, Ordering::Relaxed);
            if lane.is_none() {
                self.http2_fetches.fetch_add(1, Ordering::Relaxed);
            }
            if work.speculative {
                self.prefetches.fetch_add(1, Ordering::Relaxed);
            }
            match pack.fallback {
                Some(file)
                    if work.speculative || work.items.iter().all(|item| item.demand.ready()) =>
                {
                    self.resolve_pack_remainder(&work, scratch, git, file)?;
                    Ok(None)
                }
                Some(file) => Ok(Some(Acquired::Pack(file))),
                None => {
                    self.streamed_packs.fetch_add(1, Ordering::Relaxed);
                    Ok(None)
                }
            }
        });

        let mut queue = self.queue.lock().unwrap();
        if let Some(index) = lane {
            queue.busy[index] = false;
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
            Ok(payload) => {
                queue.mode.record_reply(started.elapsed());
                payload.map(Ok)
            }
            Err(error)
                if work.speculative
                    && !self.stop.load(Ordering::Relaxed)
                    && !object::is_cancelled(&error) =>
            {
                for item in work.items {
                    queue.retry(item, true);
                }
                self.work.notify_all();
                return;
            }
            Err(error) => {
                if !work.speculative && !object::is_cancelled(&error) {
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
        git: &Git,
    ) {
        let failure = self
            .drive_http2(&mut client, &mut wake, scratch, git)
            .err()
            .filter(|error| !object::is_cancelled(error));
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
        scratch: &RawBlobs,
        git: &Git,
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
                            client.start(token, &oids, self.response(&work, &oids, scratch))
                        }
                    };
                    match submitted {
                        Ok(()) => {
                            active.insert(token, Http2Batch { work, started });
                        }
                        Err(error) => self.finish(work, Err(error), started, None, scratch, git),
                    }
                    self.http2_peak
                        .fetch_max(active.len() as u64, Ordering::Relaxed);
                }
            }
            for (token, pack) in client.progress()? {
                let Http2Batch { work, started } = active
                    .remove(&token)
                    .context("unknown completed Git demand")?;
                self.finish(work, pack, started, None, scratch, git);
            }
            let timeout = delay.map_or(Duration::from_secs(1), |remaining| {
                remaining.max(Duration::from_millis(1))
            });
            // A failed prefetch can enqueue an exact retry from this thread.
            if delay.is_some() || self.queue.lock().unwrap().waiting.is_empty() {
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
        git: &Git,
    ) -> Result<()> {
        loop {
            let (work, failure) = {
                let mut queue = self.queue.lock().unwrap();
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
                    if let Some(work) = queue.native_work(index) {
                        queue.busy[index] = true;
                        break (work, queue.failure.clone());
                    }
                    queue = self.work.wait(queue).unwrap();
                }
            };
            self.work.notify_all();
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
                    Err(_)
                        if index != 0
                            && connection.is_ssh()
                            && !self.stop.load(Ordering::Relaxed) =>
                    {
                        // The master may have a lower MaxSessions. Nothing has
                        // been sent over this channel, so narrower reuse is safe.
                        let mut queue = self.queue.lock().unwrap();
                        queue.busy[index] = false;
                        queue.native_ceiling =
                            Some(queue.native_ceiling.unwrap_or(CONNECTIONS).min(index));
                        for item in work.items {
                            queue.retry(item, work.speculative);
                        }
                        queue.prune_prefetch();
                        self.work.notify_all();
                        return Ok(());
                    }
                    Err(error) => {
                        self.finish(work, Err(error), started, Some(index), scratch, git);
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
            self.finish(work, result, started, Some(index), scratch, git);
        }
    }
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
        for index in 0..9 {
            let oid = Oid::from_bytes(&[index; 20]).unwrap();
            queue.predicted.insert(oid);
            let expected = match index {
                0..=1 => 4,
                2..=4 => 16,
                5..=7 => 32,
                _ => 64,
            };
            assert_eq!(queue.forecast_width(oid), expected);
        }
        for width in [32, 16, 16, 4] {
            assert_eq!(queue.forecast_width(Oid::zero()), width);
        }
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
            &Git::capture(directory.path()).unwrap(),
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
    }
}
