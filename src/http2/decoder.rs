//! Decode mounted reads away from libcurl's dispatch loop.
use crate::{demand::signal_http2, object::OperationCancelled, pack_stream};
use anyhow::{Context, Result};
use curl::easy::WriteError;
use std::{
    os::unix::net::UnixStream,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
        Arc,
    },
    thread::{self, JoinHandle},
};

const QUEUE_CHUNKS: usize = 8;

pub(super) struct Decoder {
    input: Option<SyncSender<Vec<u8>>>,
    buffered: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    result: Receiver<Result<pack_stream::Received>>,
    worker: Option<JoinHandle<()>>,
    paused: bool,
}

impl Decoder {
    pub(super) fn new(
        mut pack: pack_stream::Response,
        stop: Arc<AtomicBool>,
        wake: Arc<UnixStream>,
    ) -> Result<Self> {
        let (input, chunks) = mpsc::sync_channel::<Vec<u8>>(QUEUE_CHUNKS);
        let (complete, result) = mpsc::channel();
        let buffered = Arc::new(AtomicUsize::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let pending = buffered.clone();
        let cancelled = cancel.clone();
        let worker = thread::Builder::new()
            .name("git-on-http-pack".into())
            .spawn(move || {
                let result = (|| {
                    for chunk in chunks {
                        if cancelled.load(Ordering::Acquire) || stop.load(Ordering::Relaxed) {
                            return Err(OperationCancelled.into());
                        }
                        let result = pack.write_http(&chunk);
                        // Keep bytes being classified in the count as well.
                        if pending.fetch_sub(1, Ordering::AcqRel) >= QUEUE_CHUNKS {
                            signal_http2(&wake);
                        }
                        result?;
                    }
                    if cancelled.load(Ordering::Acquire) {
                        return Err(OperationCancelled.into());
                    }
                    pack.finish_http()
                })();
                let _ = complete.send(result);
                signal_http2(&wake);
            })
            .context("start Git pack decoder")?;
        Ok(Self {
            input: Some(input),
            buffered,
            cancel,
            result,
            worker: Some(worker),
            paused: false,
        })
    }

    pub(super) fn push(&mut self, bytes: &[u8]) -> Result<usize, WriteError> {
        if self.buffered.fetch_add(1, Ordering::AcqRel) >= QUEUE_CHUNKS {
            self.buffered.fetch_sub(1, Ordering::AcqRel);
            self.paused = true;
            return Err(WriteError::Pause);
        }
        match self.input.as_ref().unwrap().try_send(bytes.to_vec()) {
            Ok(()) => Ok(bytes.len()),
            Err(error) => {
                self.buffered.fetch_sub(1, Ordering::AcqRel);
                match error {
                    mpsc::TrySendError::Full(_) => {
                        self.paused = true;
                        Err(WriteError::Pause)
                    }
                    mpsc::TrySendError::Disconnected(_) => Ok(0),
                }
            }
        }
    }

    pub(super) fn is_paused(&self) -> bool {
        self.paused
    }

    pub(super) fn has_pending_input(&self) -> bool {
        self.buffered.load(Ordering::Acquire) != 0
    }

    pub(super) fn resume(&mut self) -> bool {
        if self.paused && self.buffered.load(Ordering::Acquire) < QUEUE_CHUNKS {
            self.paused = false;
            return true;
        }
        false
    }

    pub(super) fn close(&mut self, success: bool) {
        if !success {
            self.cancel.store(true, Ordering::Release);
        }
        self.input.take();
    }

    pub(super) fn result(&self) -> Option<Result<pack_stream::Received>> {
        match self.result.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                Some(Err(anyhow::anyhow!("Git pack decoder exited")))
            }
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.close(false);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
#[test]
fn a_failed_decoder_retires_a_paused_http_stream() {
    use super::{Body, Client};
    use curl::multi::Multi;

    let (complete, result) = mpsc::channel();
    complete
        .send(Err(std::io::Error::from_raw_os_error(libc::ENOSPC).into()))
        .unwrap();
    let decoder = Decoder {
        input: None,
        buffered: Arc::new(AtomicUsize::new(QUEUE_CHUNKS)),
        cancel: Arc::default(),
        result,
        worker: None,
        paused: true,
    };
    let mut client = Client {
        base: "https://example.invalid/".parse().unwrap(),
        ca: None,
        extra_header: None,
        user_agent: None,
        auth: None,
        credential: None,
        multi: Multi::new(),
        active: Default::default(),
        decoding: Default::default(),
        stop: Arc::default(),
    };
    client
        .start_request(7, Vec::new(), Body::Streaming(decoder))
        .unwrap();
    let mut completed = client.poll_active_decoders().unwrap();
    assert!(client.active.is_empty());
    let (token, result) = completed.pop().unwrap();
    assert_eq!(token, 7);
    let error = result.err().unwrap().downcast::<std::io::Error>().unwrap();
    assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
}
