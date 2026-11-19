//! Bounded HTTPS demand scheduling. Workers receive checked pack bytes and
//! leave publication to the snapshot's single object-store writer.
use crate::{object, raw_blob::RawBlobs, spool::Spool, transport::Session};
use anyhow::{bail, Context, Result};
use git2::Oid;
use std::{
    collections::{HashMap, VecDeque},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(crate) const CONNECTIONS: usize = 8;
const INITIAL_CONNECTIONS: usize = 4;
const MAX_OUTSTANDING: usize = 32;
const MAX_WANTS: usize = 8;
const REPLY_SAMPLES: u8 = 8;
const SLOW_REPLY: Duration = Duration::from_millis(64);
const BATCH_TARGET: usize = 4;
const BATCH_WINDOW: Duration = Duration::from_millis(2);

pub(crate) struct Batch {
    pub(crate) oids: Vec<Oid>,
    pack: Mutex<Option<Result<Spool>>>,
}

impl Batch {
    fn new(oids: Vec<Oid>, pack: Result<Spool>) -> Arc<Self> {
        Arc::new(Self {
            oids,
            pack: Mutex::new(Some(pack)),
        })
    }

    pub(crate) fn take_pack(&self) -> Option<Result<Spool>> {
        self.pack.lock().unwrap().take()
    }
}

#[derive(Clone, Default)]
pub(crate) struct Demand(Arc<Flight>);

#[derive(Default)]
struct Flight {
    state: Mutex<FlightState>,
    ready: Condvar,
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

    #[cfg(target_os = "macos")]
    pub(crate) fn ready(&self) -> bool {
        self.0.state.lock().unwrap().batch.is_some()
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
    fn record_reply(&mut self, elapsed: Duration) -> bool {
        let Self::Probing(samples) = self else {
            return false;
        };
        // A fast reply rules out consistently high startup latency. Later
        // bursts of slow replies can reflect server load or larger packs.
        if elapsed < SLOW_REPLY {
            *self = Self::Standard;
        } else {
            *samples += 1;
            if *samples == REPLY_SAMPLES {
                *self = Self::Wide;
            }
        }
        !matches!(self, Self::Probing(_))
    }
}

#[derive(Default)]
struct Queue {
    waiting: VecDeque<QueuedDemand>,
    pending: HashMap<Oid, Demand>,
    outstanding: usize,
    busy: [bool; CONNECTIONS],
    failure: Option<String>,
    mode: ConnectionMode,
}

impl Queue {
    fn batch_wait(&self, now: Instant) -> Option<Duration> {
        if matches!(self.mode, ConnectionMode::Wide)
            && self.waiting.len() < BATCH_TARGET
            && self.busy.contains(&true)
        {
            BATCH_WINDOW
                .checked_sub(now.duration_since(self.waiting.front()?.queued_at))
                .filter(|remaining| !remaining.is_zero())
        } else {
            None
        }
    }
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    work: Condvar,
    mode_ready: Condvar,
    stop: Arc<AtomicBool>,
    additional_starts: AtomicU64,
}

pub(crate) struct Pool {
    shared: Arc<Shared>,
    workers: Mutex<Vec<JoinHandle<Result<()>>>>,
}

impl Pool {
    pub(crate) fn new(
        mut first: Session,
        additional: Vec<Command>,
        scratch: &RawBlobs,
    ) -> Result<Self> {
        let mut pool = Self {
            shared: Arc::default(),
            workers: Mutex::default(),
        };
        first.set_stop(pool.shared.stop.clone());
        pool.start(0, Some(first), None, scratch)?;
        for (index, command) in additional.into_iter().enumerate() {
            pool.start(index + 1, None, Some(command), scratch)?;
        }
        Ok(pool)
    }

    fn start(
        &mut self,
        index: usize,
        session: Option<Session>,
        command: Option<Command>,
        scratch: &RawBlobs,
    ) -> Result<()> {
        let shared = self.shared.clone();
        let scratch = scratch.clone();
        let worker = thread::Builder::new()
            .name(format!("git-on-https-{index}"))
            .spawn(move || shared.worker(index, session, command, &scratch))?;
        self.workers.get_mut().unwrap().push(worker);
        Ok(())
    }

    pub(crate) fn request(&self, oid: Oid) -> Result<Demand> {
        object::check_transfer()?;
        let mut queue = self.shared.queue.lock().unwrap();
        loop {
            if let Some(demand) = queue.pending.get(&oid) {
                return Ok(demand.clone());
            }
            if self.shared.stop.load(Ordering::Relaxed) {
                return Err(object::OperationCancelled.into());
            }
            if let Some(error) = &queue.failure {
                bail!("remote object transport previously failed: {error}");
            }
            if queue.outstanding < MAX_OUTSTANDING {
                let demand = Demand::default();
                queue.pending.insert(oid, demand.clone());
                queue.waiting.push_back(QueuedDemand {
                    oid,
                    demand: demand.clone(),
                    queued_at: Instant::now(),
                });
                queue.outstanding += 1;
                self.shared.work.notify_all();
                return Ok(demand);
            }
            queue = self.shared.work.wait(queue).unwrap();
            object::check_transfer()?;
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

    pub(crate) fn stop(&self) -> Result<()> {
        let waiting = {
            let mut queue = self.shared.queue.lock().unwrap();
            self.shared.stop.store(true, Ordering::Relaxed);
            std::mem::take(&mut queue.waiting)
        };
        self.shared.work.notify_all();
        self.shared.mode_ready.notify_all();
        for QueuedDemand { oid, demand, .. } in waiting {
            demand.complete(Batch::new(
                vec![oid],
                Err(object::OperationCancelled.into()),
            ));
        }
        let mut outcome = Ok(());
        for worker in self.workers.lock().unwrap().drain(..) {
            let result = worker
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("HTTPS worker panicked")));
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
    fn worker(
        &self,
        index: usize,
        mut session: Option<Session>,
        mut command: Option<Command>,
        scratch: &RawBlobs,
    ) -> Result<()> {
        if index >= INITIAL_CONNECTIONS {
            // Spare workers do not compete for the work queue during probing.
            let queue = self.queue.lock().unwrap();
            let queue = self
                .mode_ready
                .wait_while(queue, |queue| {
                    !self.stop.load(Ordering::Relaxed)
                        && matches!(queue.mode, ConnectionMode::Probing(_))
                })
                .unwrap();
            if !matches!(queue.mode, ConnectionMode::Wide) {
                return Ok(());
            }
        }
        loop {
            let next = {
                let mut queue = self.queue.lock().unwrap();
                loop {
                    queue = self
                        .work
                        .wait_while(queue, |queue| {
                            !self.stop.load(Ordering::Relaxed)
                                && (queue.waiting.is_empty()
                                    || queue.busy[..index].contains(&false))
                        })
                        .unwrap();
                    if self.stop.load(Ordering::Relaxed) {
                        break None;
                    }
                    if let Some(remaining) = queue.batch_wait(Instant::now()) {
                        queue = self.work.wait_timeout(queue, remaining).unwrap().0;
                        continue;
                    }
                    // Use an established lower-numbered connection first.
                    let count = queue.waiting.len().min(MAX_WANTS);
                    let work: Vec<_> = queue.waiting.drain(..count).collect();
                    queue.busy[index] = true;
                    break Some((work, queue.failure.clone()));
                }
            };
            let Some((work, failure)) = next else {
                break;
            };
            self.work.notify_all();
            let oids = work.iter().map(|item| item.oid).collect::<Vec<_>>();
            let result = (|| {
                if let Some(failure) = failure {
                    bail!("remote object transport previously failed: {failure}");
                }
                object::check_transfer()?;
                if session.is_none() {
                    self.additional_starts.fetch_add(1, Ordering::Relaxed);
                    let command = command.take().context("missing HTTPS helper command")?;
                    session = Session::connect_with_stop(command, true, Some(self.stop.clone()))?;
                }
                let session = session
                    .as_mut()
                    .context("HTTPS helper cannot reuse its connection")?;
                let pack = scratch.scratch_file()?;
                let started = Instant::now();
                session.fetch_exact(&oids, &pack)?;
                Ok((pack, started.elapsed()))
            })();
            let result = {
                let mut queue = self.queue.lock().unwrap();
                let result = result.map(|(pack, elapsed)| {
                    if queue.mode.record_reply(elapsed) {
                        self.mode_ready.notify_all();
                    }
                    pack
                });
                if let Err(error) = &result {
                    if queue.failure.is_none() && !object::is_cancelled(error) {
                        queue.failure = Some(format!("{error:#}"));
                    }
                }
                queue.outstanding -= work.len();
                queue.busy[index] = false;
                result
            };
            let batch = Batch::new(oids, result);
            for item in work {
                item.demand.complete(batch.clone());
            }
            self.work.notify_all();
        }
        session.as_ref().map_or(Ok(()), Session::stop)
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
            assert!(!slow.record_reply(SLOW_REPLY));
            assert!(!variable.record_reply(SLOW_REPLY));
        }
        assert!(variable.record_reply(SLOW_REPLY / 2));
        for _ in 0..REPLY_SAMPLES {
            assert!(!variable.record_reply(SLOW_REPLY));
        }
        assert!(matches!(variable, ConnectionMode::Standard));
        assert!(slow.record_reply(SLOW_REPLY));
        assert!(!slow.record_reply(SLOW_REPLY / 2));
        assert!(matches!(slow, ConnectionMode::Wide));
    }

    #[test]
    fn batch_waits_for_more_demands_until_target_or_deadline() {
        let now = Instant::now();
        let mut queue = Queue::default();
        queue.waiting.push_back(QueuedDemand {
            oid: Oid::zero(),
            demand: Demand::default(),
            queued_at: now,
        });
        queue.busy[0] = true;
        assert_eq!(queue.batch_wait(now), None);
        queue.mode = ConnectionMode::Wide;
        assert_eq!(queue.batch_wait(now), Some(BATCH_WINDOW));
        assert_eq!(queue.batch_wait(now + BATCH_WINDOW), None);
        queue.busy[0] = false;
        assert_eq!(queue.batch_wait(now), None);
        queue.busy[0] = true;
        for index in 1..BATCH_TARGET {
            assert_eq!(queue.batch_wait(now), Some(BATCH_WINDOW));
            queue.waiting.push_back(QueuedDemand {
                oid: Oid::from_bytes(&[index as u8; 20]).unwrap(),
                demand: Demand::default(),
                queued_at: now,
            });
        }
        assert_eq!(queue.batch_wait(now), None);
    }
}
