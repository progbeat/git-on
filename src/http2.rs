//! Git protocol v2 exact-object requests multiplexed by an invocation-owned
//! libcurl multi handle. Native Git still selects and verifies the revision.
use crate::{
    object::{check_transfer, OperationCancelled},
    pack_stream, transport,
};
use anyhow::{ensure, Context, Result};
use curl::{
    easy::{Auth, Easy2, Handler, HttpVersion, List, WriteError},
    multi::{Easy2Handle, Multi, WaitFd},
};
use git2::Oid;
use std::{
    collections::HashMap,
    ffi::OsStr,
    io::{self, Read},
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const ADVERTISEMENT: &[u8] = b"application/x-git-upload-pack-advertisement";
const PACK_RESULT: &[u8] = b"application/x-git-upload-pack-result";

pub(crate) struct Configuration {
    base: String,
    ca: Option<PathBuf>,
    credential: Command,
}

impl Configuration {
    pub(crate) fn new(url: &OsStr, credential: Command) -> Option<Self> {
        let url = url.to_str()?;
        let parsed = url::Url::parse(url).ok()?;
        let version = curl::Version::get();
        if parsed.scheme() != "https"
            || parsed.as_str() != url
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || !version.feature_ssl()
            || !version.feature_http2()
        {
            return None;
        }
        Some(Self {
            base: url.trim_end_matches('/').to_owned(),
            ca: std::env::var_os("GIT_SSL_CAINFO").map(PathBuf::from),
            credential,
        })
    }
}

struct Credentials {
    user: String,
    password: String,
}

fn credentials(mut command: Command, url: &str) -> Result<Option<Credentials>> {
    command
        .args(["-c", "credential.interactive=false", "credential", "fill"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "/bin/false")
        .env("SSH_ASKPASS", "/bin/false");
    let request = format!("url={url}\n\n");
    let (output, values) =
        crate::git::run_with_output(command, Some(&mut request.as_bytes()), |stream| {
            let mut values = Vec::new();
            stream.take(65_537).read_to_end(&mut values)?;
            ensure!(
                values.len() <= 65_536,
                "Git credential response is oversized"
            );
            Ok(values)
        })?;
    if !output.status.success() {
        return Ok(None);
    }
    let mut user = None;
    let mut password = None;
    for line in values.split(|&byte| byte == b'\n') {
        if let Some(value) = line.strip_prefix(b"username=") {
            user = std::str::from_utf8(value)
                .ok()
                .filter(|v| !v.contains('\0'));
        } else if let Some(value) = line.strip_prefix(b"password=") {
            password = std::str::from_utf8(value)
                .ok()
                .filter(|v| !v.contains('\0'));
        }
    }
    Ok(user.zip(password).map(|(user, password)| Credentials {
        user: user.to_owned(),
        password: password.to_owned(),
    }))
}

// Bound queued wire bytes per receiving stream independently of the number
// of requests libcurl can multiplex. A full queue pauses only that transfer.
const QUEUE_CHUNKS: usize = 8;

pub(crate) type ReportFailure = Arc<dyn Fn(&anyhow::Error) + Send + Sync>;

fn note_failure<T>(report: Option<&ReportFailure>, result: &Result<T>) {
    if let (Some(report), Err(error)) = (report, result) {
        if !crate::object::is_cancelled(error) {
            report(error);
        }
    }
}

enum Body {
    Discovery(Vec<u8>),
    PendingReply {
        pack: Box<pack_stream::Response>,
        wake: Arc<UnixStream>,
        report_failure: Option<ReportFailure>,
    },
    Reply(Decoder),
}

struct Decoder {
    input: Option<SyncSender<Vec<u8>>>,
    buffered: Arc<AtomicUsize>,
    transport: Arc<Mutex<Option<Result<()>>>>,
    report_failure: Option<ReportFailure>,
    result: Receiver<Result<pack_stream::Received>>,
    thread: Option<JoinHandle<()>>,
    paused: bool,
}

impl Decoder {
    fn start(
        mut pack: Box<pack_stream::Response>,
        stop: Arc<AtomicBool>,
        wake: Arc<UnixStream>,
        report_failure: Option<ReportFailure>,
    ) -> io::Result<Self> {
        let (input, chunks) = mpsc::sync_channel::<Vec<u8>>(QUEUE_CHUNKS);
        let (complete, result) = mpsc::channel();
        let buffered = Arc::new(AtomicUsize::new(0));
        let transport = Arc::new(Mutex::new(None::<Result<()>>));
        let pending = buffered.clone();
        let outcome = transport.clone();
        let report = report_failure.clone();
        let thread = thread::Builder::new()
            .name("git-on-http-pack".into())
            .spawn(move || {
                let received = (|| {
                    for chunk in &chunks {
                        if pending.fetch_sub(1, Ordering::AcqRel) >= QUEUE_CHUNKS {
                            crate::demand::signal_http2(&wake);
                        }
                        if stop.load(Ordering::Relaxed) {
                            return Err(OperationCancelled.into());
                        }
                        pack.write_http(&chunk)?;
                    }
                    outcome
                        .lock()
                        .unwrap()
                        .take()
                        .unwrap_or_else(|| Err(OperationCancelled.into()))?;
                    pack.finish_http()
                })();
                note_failure(report.as_ref(), &received);
                let _ = complete.send(received);
                crate::demand::signal_http2(&wake);
            })?;
        Ok(Self {
            input: Some(input),
            buffered,
            transport,
            report_failure,
            result,
            thread: Some(thread),
            paused: false,
        })
    }

    fn feed(&mut self, data: &[u8]) -> Result<usize, WriteError> {
        self.buffered.fetch_add(1, Ordering::AcqRel);
        match self
            .input
            .as_ref()
            .expect("active HTTP reply")
            .try_send(data.to_vec())
        {
            Ok(()) => Ok(data.len()),
            Err(error) => {
                self.buffered.fetch_sub(1, Ordering::AcqRel);
                match error {
                    TrySendError::Full(_) => {
                        self.paused = true;
                        Err(WriteError::Pause)
                    }
                    TrySendError::Disconnected(_) => Ok(0),
                }
            }
        }
    }

    fn can_resume(&mut self) -> bool {
        if self.paused && self.buffered.load(Ordering::Acquire) < QUEUE_CHUNKS {
            self.paused = false;
            true
        } else {
            false
        }
    }

    fn end(&mut self, outcome: Result<()>) {
        note_failure(self.report_failure.as_ref(), &outcome);
        *self.transport.lock().unwrap() = Some(outcome);
        drop(self.input.take());
    }

    fn result(&self) -> Option<Result<pack_stream::Received>> {
        match self.result.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                Some(Err(anyhow::anyhow!("Git HTTPS decoder stopped")))
            }
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        drop(self.input.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Response {
    body: Body,
    error: Option<io::Error>,
    stop: Arc<AtomicBool>,
    h2: bool,
    ok: bool,
    git_pack: bool,
    basic: bool,
}

impl Handler for Response {
    fn write(&mut self, data: &[u8]) -> Result<usize, WriteError> {
        if self.h2 && self.ok && self.git_pack && matches!(self.body, Body::PendingReply { .. }) {
            let body = std::mem::replace(&mut self.body, Body::Discovery(Vec::new()));
            let Body::PendingReply {
                pack,
                wake,
                report_failure,
            } = body
            else {
                unreachable!()
            };
            match Decoder::start(pack, self.stop.clone(), wake, report_failure) {
                Ok(decoder) => self.body = Body::Reply(decoder),
                Err(error) => {
                    self.error = Some(error);
                    return Ok(0);
                }
            }
        }
        match &mut self.body {
            Body::Discovery(bytes) if bytes.len() + data.len() <= 256 * 1024 => {
                bytes.extend_from_slice(data);
                Ok(data.len())
            }
            Body::Discovery(_) => {
                self.error = Some(io::Error::other("Git HTTP discovery response is oversized"));
                Ok(0)
            }
            Body::Reply(decoder) if self.h2 && self.ok && self.git_pack => decoder.feed(data),
            Body::Reply(_) | Body::PendingReply { .. } => Ok(data.len()),
        }
    }

    fn header(&mut self, data: &[u8]) -> bool {
        if data.starts_with(b"HTTP/") {
            self.h2 = data.starts_with(b"HTTP/2 ");
            self.ok = data.split(u8::is_ascii_whitespace).nth(1) == Some(b"200");
            self.git_pack = false;
            self.basic = false;
            if let Body::Discovery(bytes) = &mut self.body {
                bytes.clear();
            }
        } else if let Some(colon) = data.iter().position(|&byte| byte == b':') {
            if data[..colon].eq_ignore_ascii_case(b"content-type") {
                self.git_pack = data[colon + 1..].trim_ascii() == PACK_RESULT;
            }
            if data[..colon].eq_ignore_ascii_case(b"www-authenticate") {
                self.basic |= data[colon + 1..]
                    .split(u8::is_ascii_whitespace)
                    .find(|word| !word.is_empty())
                    .is_some_and(|scheme| scheme.eq_ignore_ascii_case(b"basic"));
            }
        }
        true
    }

    fn progress(&mut self, _: f64, _: f64, _: f64, _: f64) -> bool {
        !self.stop.load(Ordering::Relaxed) && check_transfer().is_ok()
    }
}

pub(crate) struct Client {
    base: String,
    ca: Option<PathBuf>,
    auth: Option<Credentials>,
    multi: Multi,
    active: HashMap<usize, Easy2Handle<Response>>,
    decoding: HashMap<usize, Decoder>,
    stop: Arc<AtomicBool>,
}

impl Client {
    pub(crate) fn negotiate(
        config: Configuration,
        stop: Arc<AtomicBool>,
        branch: Option<&transport::BranchQuery>,
    ) -> Result<Option<(Self, Option<git2::Oid>)>> {
        let mut multi = Multi::new();
        multi.pipelining(false, true)?;
        // All submitted requests share the connection. libcurl applies the
        // peer's HTTP/2 stream and flow-control limits.
        multi.set_max_host_connections(1)?;
        let mut client = Self {
            base: config.base,
            ca: config.ca,
            auth: None,
            multi,
            active: HashMap::new(),
            decoding: HashMap::new(),
            stop,
        };
        let mut credential = Some(config.credential);
        let response =
            client.discovery("info/refs?service=git-upload-pack", None, &mut credential)?;
        if response.response_code()? != 200
            || !response.get_ref().h2
            || response.content_type_bytes()? != Some(ADVERTISEMENT)
        {
            return Ok(None);
        }
        let Body::Discovery(bytes) = &response.get_ref().body else {
            unreachable!()
        };
        if !transport::read_http_capabilities(bytes)? {
            return Ok(None);
        }
        // Some servers advertise capabilities publicly and authenticate
        // upload-pack POSTs. Negotiate that exchange before sending wants.
        let response = client.discovery(
            "git-upload-pack",
            Some(transport::reference_request(branch)?),
            &mut credential,
        )?;
        if response.response_code()? != 200
            || !response.get_ref().h2
            || response.content_type_bytes()? != Some(PACK_RESULT)
        {
            return Ok(None);
        }
        let Body::Discovery(bytes) = &response.get_ref().body else {
            unreachable!()
        };
        let selected = transport::read_http_branch(bytes, branch)?;
        Ok((branch.is_none() || selected.is_some()).then_some((client, selected)))
    }

    fn easy(&self, service: &str, body: Body, request: Option<Vec<u8>>) -> Result<Easy2<Response>> {
        let mut easy = Easy2::new(Response {
            body,
            error: None,
            stop: self.stop.clone(),
            h2: false,
            ok: false,
            git_pack: false,
            basic: false,
        });
        easy.url(&format!("{}/{service}", self.base))?;
        easy.http_version(HttpVersion::V2TLS)?;
        easy.pipewait(true)?;
        easy.progress(true)?;
        if let Some(ca) = &self.ca {
            easy.cainfo(ca)?;
        }
        if let Some(auth) = &self.auth {
            easy.username(&auth.user)?;
            easy.password(&auth.password)?;
            easy.http_auth(Auth::new().basic(true))?;
        }
        let mut headers = List::new();
        headers.append("Git-Protocol: version=2")?;
        if let Some(request) = request {
            headers.append("Content-Type: application/x-git-upload-pack-request")?;
            headers.append("Accept: application/x-git-upload-pack-result")?;
            easy.post_fields_copy(&request)?;
        }
        easy.http_headers(headers)?;
        Ok(easy)
    }

    fn discovery(
        &mut self,
        service: &str,
        request: Option<Vec<u8>>,
        credential: &mut Option<Command>,
    ) -> Result<Easy2<Response>> {
        let response = self.metadata(service, request.clone())?;
        if response.response_code()? == 401 && response.get_ref().h2 && response.get_ref().basic {
            if let Some(command) = credential.take() {
                self.auth = credentials(command, &self.base)?;
                if self.auth.is_some() {
                    return self.metadata(service, request);
                }
            }
        }
        Ok(response)
    }

    fn metadata(&self, service: &str, request: Option<Vec<u8>>) -> Result<Easy2<Response>> {
        let easy = self.easy(service, Body::Discovery(Vec::new()), request)?;
        let handle = self.multi.add2(easy)?;
        loop {
            check_transfer()?;
            let running = self.multi.perform()?;
            let mut done = None;
            self.multi.messages(|message| {
                done = message.result_for2(&handle);
            });
            if let Some(result) = done {
                let mut easy = self.multi.remove2(handle)?;
                if let Some(error) = easy.get_mut().error.take() {
                    return Err(error.into());
                }
                result?;
                return Ok(easy);
            }
            ensure!(running != 0, "Git HTTPS discovery ended without a result");
            self.multi.wait(&mut [], Duration::from_millis(100))?;
        }
    }

    pub(crate) fn start(
        &mut self,
        token: usize,
        oids: &[Oid],
        response: pack_stream::Response,
        wake: &Arc<UnixStream>,
        report_failure: Option<ReportFailure>,
    ) -> Result<()> {
        let easy = self.easy(
            "git-upload-pack",
            Body::PendingReply {
                pack: Box::new(response),
                wake: wake.clone(),
                report_failure,
            },
            Some(transport::exact_request(oids)?),
        )?;
        let mut handle = self.multi.add2(easy)?;
        handle.set_token(token)?;
        self.active.insert(token, handle);
        Ok(())
    }

    pub(crate) fn progress(&mut self) -> Result<Vec<(usize, Result<pack_stream::Received>)>> {
        let mut failed_resume = Vec::new();
        for (&token, handle) in &mut self.active {
            if let Body::Reply(decoder) = &mut handle.get_mut().body {
                if decoder.can_resume() {
                    // libcurl can call the body callback during unpause. A
                    // decoder rejection here belongs to this stream alone.
                    if let Err(error) = handle.unpause_write() {
                        failed_resume.push((token, error));
                    }
                }
            }
        }

        let mut finished = Vec::new();
        for (token, error) in failed_resume {
            self.end_transfer(token, Err(error), &mut finished)?;
        }
        self.multi.perform()?;
        let mut transfers = Vec::new();
        self.multi.messages(|message| {
            if let Some(result) = message.result() {
                transfers.push((message.token(), result));
            }
        });
        for (token, outcome) in transfers {
            self.end_transfer(token?, outcome, &mut finished)?;
        }

        // A decoder can reject a reply while its peer is still sending.
        // Drop only that transfer and deliver the decoder's diagnostic.
        let rejected: Vec<_> = self
            .active
            .iter()
            .filter_map(|(&token, handle)| match &handle.get_ref().body {
                Body::Reply(decoder) => decoder.result().map(|result| (token, result)),
                _ => None,
            })
            .collect();
        for (token, result) in rejected {
            let handle = self
                .active
                .remove(&token)
                .expect("active Git HTTP/2 request");
            self.multi.remove2(handle)?;
            finished.push((token, result));
        }

        self.decoding.retain(|&token, decoder| {
            if let Some(result) = decoder.result() {
                finished.push((token, result));
                false
            } else {
                true
            }
        });
        Ok(finished)
    }

    fn end_transfer(
        &mut self,
        token: usize,
        outcome: Result<(), curl::Error>,
        finished: &mut Vec<(usize, Result<pack_stream::Received>)>,
    ) -> Result<()> {
        let handle = self
            .active
            .remove(&token)
            .context("unknown Git HTTP/2 request")?;
        let mut response = self.multi.remove2(handle)?;
        let network = self.check_response(&mut response, outcome);
        let body = std::mem::replace(&mut response.get_mut().body, Body::Discovery(Vec::new()));
        match body {
            Body::Reply(mut decoder) => {
                decoder.end(network);
                self.decoding.insert(token, decoder);
            }
            Body::PendingReply { pack, .. } => {
                finished.push((token, network.and_then(|()| pack.finish_http())));
            }
            Body::Discovery(_) => {
                let result =
                    network.and_then(|()| Err(anyhow::anyhow!("missing Git HTTP/2 fetch body")));
                finished.push((token, result));
            }
        }
        Ok(())
    }

    fn check_response(
        &self,
        response: &mut Easy2<Response>,
        outcome: Result<(), curl::Error>,
    ) -> Result<()> {
        if let Some(error) = response.get_mut().error.take() {
            return Err(error.into());
        }
        if self.stop.load(Ordering::Relaxed) {
            return Err(OperationCancelled.into());
        }
        check_transfer()?;
        outcome.context("receive Git HTTP/2 response")?;
        let status = response.response_code()?;
        ensure!(status == 200, "Git HTTP/2 fetch returned HTTP {status}");
        ensure!(
            response.get_ref().h2 && response.content_type_bytes()? == Some(PACK_RESULT),
            "invalid Git HTTP/2 fetch response"
        );
        Ok(())
    }

    pub(crate) fn wait(&self, wake: &UnixStream, timeout: Duration) -> Result<()> {
        let mut fd = WaitFd::new();
        fd.set_fd(wake.as_raw_fd());
        fd.poll_on_read(true);
        self.multi.wait(&mut [fd], timeout)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, sync::mpsc, thread};

    fn framed_blob() -> (Oid, Vec<u8>, Vec<u8>) {
        use sha1_checked::{Digest, Sha1};

        let oid = Oid::hash_object(git2::ObjectType::Blob, b"x").unwrap();
        let mut encoder = flate2::write::ZlibEncoder::new(
            b"PACK\0\0\0\x02\0\0\0\x01\x31".to_vec(),
            flate2::Compression::fast(),
        );
        encoder.write_all(b"x").unwrap();
        let pack = encoder.finish().unwrap();
        let mut frame = format!("000dpackfile\n{:04x}\x01", pack.len() + 5).into_bytes();
        frame.extend_from_slice(&pack);
        let trailer = Sha1::digest(&pack);
        let mut tail = format!("{:04x}\x01", trailer.len() + 5).into_bytes();
        tail.extend_from_slice(&trailer);
        tail.extend_from_slice(b"0000");
        (oid, frame, tail)
    }

    #[test]
    fn decoded_pack_failure_is_reported_without_another_dispatch_pass() {
        let (oid, frame, _) = framed_blob();
        let directory = tempfile::tempdir().unwrap();
        let scratch = crate::raw_blob::RawBlobs::new(directory.path()).unwrap();
        let (publish, published) = mpsc::channel();
        let pack = pack_stream::Response::new(
            scratch,
            vec![oid],
            Box::new(move |object| {
                assert_eq!(object.oid(), oid);
                publish.send(())?;
                Ok(())
            }),
        );
        let (_read, wake) = UnixStream::pair().unwrap();
        wake.set_nonblocking(true).unwrap();
        let (report, reported) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let resume = Mutex::new(resume);
        let mut decoder = Decoder::start(
            Box::new(pack),
            Arc::default(),
            Arc::new(wake),
            Some(Arc::new(move |error| {
                let _ = report.send(error.to_string());
                resume.lock().unwrap().recv().unwrap();
            })),
        )
        .unwrap();
        decoder.feed(&frame).unwrap();
        published.recv_timeout(Duration::from_secs(3)).unwrap();
        decoder.feed(b"zzzz").unwrap();
        let diagnostic = reported.recv_timeout(Duration::from_secs(3));
        let accepted = decoder.feed(b"0000");
        let _ = release.send(());
        let result = decoder.result.recv_timeout(Duration::from_secs(3)).unwrap();
        let expected = result.err().expect("malformed packet").to_string();
        drop(decoder);
        assert_eq!(
            diagnostic.expect("decoder discarded a diagnosed error"),
            expected
        );
        assert_eq!(
            accepted.unwrap(),
            4,
            "libcurl saw a write error before the decoder's report finished"
        );
    }

    #[test]
    fn terminal_transport_failure_survives_decoder_cancellation() {
        let (oid, frame, _) = framed_blob();
        let directory = tempfile::tempdir().unwrap();
        let scratch = crate::raw_blob::RawBlobs::new(directory.path()).unwrap();
        let (publish, published) = mpsc::channel();
        let (release, resumed) = mpsc::channel();
        let pack = pack_stream::Response::new(
            scratch,
            vec![oid],
            Box::new(move |_| {
                publish.send(())?;
                resumed.recv()?;
                Ok(())
            }),
        );
        let (_read, wake) = UnixStream::pair().unwrap();
        wake.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (report, reported) = mpsc::channel();
        let mut decoder = Decoder::start(
            Box::new(pack),
            stop.clone(),
            Arc::new(wake),
            Some(Arc::new(move |error| {
                let _ = report.send(error.to_string());
            })),
        )
        .unwrap();
        decoder.feed(&frame).unwrap();
        published.recv_timeout(Duration::from_secs(3)).unwrap();
        decoder.feed(b"0000").unwrap();
        decoder.end(Err(anyhow::anyhow!("required transfer was reset")));
        stop.store(true, Ordering::Relaxed);
        release.send(()).unwrap();
        let result = decoder.result.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(result.as_ref().is_err_and(crate::object::is_cancelled));
        drop(decoder);
        assert_eq!(reported.try_recv().unwrap(), "required transfer was reset");
    }

    #[test]
    fn blocked_decode_does_not_hold_an_independent_reply() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = crate::raw_blob::RawBlobs::new(directory.path()).unwrap();
        let (oid, frame, tail) = framed_blob();
        let (started, wait_for_start) = mpsc::channel();
        let (release, wait_for_release) = mpsc::channel();
        let (published, wait_for_publication) = mpsc::channel();
        let (paused, wait_for_pause) = mpsc::channel();
        let (_read, wake) = UnixStream::pair().unwrap();
        wake.set_nonblocking(true).unwrap();
        let wake = Arc::new(wake);
        let handler = |publish| Response {
            body: Body::PendingReply {
                pack: Box::new(pack_stream::Response::new(
                    scratch.clone(),
                    vec![oid],
                    publish,
                )),
                wake: wake.clone(),
                report_failure: None,
            },
            error: None,
            stop: Arc::default(),
            h2: true,
            ok: true,
            git_pack: true,
            basic: false,
        };
        let mut slow = handler(Box::new(move |_| {
            started.send(())?;
            wait_for_release.recv()?;
            Ok(())
        }));
        let mut independent = handler(Box::new(move |object| {
            assert_eq!(object.oid(), oid);
            let _ = published.send(());
            Ok(())
        }));
        thread::scope(|scope| {
            scope.spawn(move || {
                assert_eq!(slow.write(&frame).unwrap(), frame.len());
                let Body::Reply(decoder) = &mut slow.body else {
                    unreachable!();
                };
                let deadline = std::time::Instant::now() + Duration::from_secs(3);
                while decoder.buffered.load(Ordering::Acquire) != 0 {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "decoder never started"
                    );
                    thread::yield_now();
                }
                // The callback is still blocked on the first object. Fill its
                // wire queue, then replay the refused bytes after it drains.
                for byte in &tail[..QUEUE_CHUNKS] {
                    assert_eq!(decoder.feed(std::slice::from_ref(byte)).unwrap(), 1);
                }
                let replay = &tail[QUEUE_CHUNKS..];
                assert!(matches!(decoder.feed(replay), Err(WriteError::Pause)));
                assert!(!decoder.can_resume());
                let _ = paused.send(());
                assert_eq!(independent.write(&frame).unwrap(), frame.len());
                let deadline = std::time::Instant::now() + Duration::from_secs(3);
                while !decoder.can_resume() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "decoder stayed paused"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
                assert_eq!(decoder.feed(replay).unwrap(), replay.len());
                decoder.end(Ok(()));
                let received = decoder
                    .result
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                assert!(received.fallback.is_none());
            });
            let entered = wait_for_start.recv_timeout(Duration::from_secs(3));
            let bounded = wait_for_pause.recv_timeout(Duration::from_secs(3));
            let outcome = wait_for_publication.recv_timeout(Duration::from_secs(3));
            let _ = release.send(());
            entered.expect("the first HTTP response did not reach its decoder");
            bounded.expect("the blocked stream did not reach its queue limit");
            outcome.expect("a blocked pack decoder held an independent HTTP response");
        });
    }
}
