//! Git protocol v2 exact-object requests multiplexed by an invocation-owned
//! libcurl multi handle. Git provides credential lookup and signature verification.
use crate::{
    object::{check_transfer, OperationCancelled},
    pack_stream, transport,
};
use anyhow::{ensure, Context, Result};
use curl::{
    easy::{Auth, Easy2, Handler, HttpVersion, List, NetRc, WriteError},
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
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const ADVERTISEMENT: &[u8] = b"application/x-git-upload-pack-advertisement";
const PACK_RESULT: &[u8] = b"application/x-git-upload-pack-result";

fn content_type_is(value: Option<&[u8]>, expected: &[u8]) -> bool {
    value
        .and_then(|value| value.split(|&byte| byte == b';').next())
        .is_some_and(|value| value.trim_ascii().eq_ignore_ascii_case(expected))
}

type CredentialProvider = Box<dyn FnOnce(&str, &[Vec<u8>]) -> Result<Credentials> + Send>;

pub(crate) struct Configuration {
    base: String,
    ca: Option<PathBuf>,
    user_agent: String,
    credential: CredentialProvider,
}

impl Configuration {
    pub(crate) fn new(
        url: &OsStr,
        git_version: &str,
        credential: impl FnOnce(&str, &[Vec<u8>]) -> Result<Credentials> + Send + 'static,
    ) -> Option<Self> {
        let url = url.to_str()?;
        let parsed = url::Url::parse(url).ok()?;
        let version = curl::Version::get();
        if parsed.scheme() != "https"
            || parsed.as_str() != url
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || !git_version
                .bytes()
                .all(|byte| byte == b' ' || byte.is_ascii_graphic())
            || !version.feature_ssl()
            || !version.feature_http2()
        {
            return None;
        }
        Some(Self {
            // Service paths add one slash. Preserve any extra slashes in
            // the repository path, as Git's HTTP transport does.
            base: url.strip_suffix('/').unwrap_or(url).to_owned(),
            ca: std::env::var_os("GIT_SSL_CAINFO").map(PathBuf::from),
            // Reuse the startup version probe for Git's standard HTTP header.
            user_agent: format!("git/{git_version}"),
            credential: Box::new(credential),
        })
    }
}

pub(crate) struct Credentials {
    user: String,
    password: String,
}

pub(crate) fn credentials(
    mut command: Command,
    url: &str,
    challenges: &[Vec<u8>],
) -> Result<Credentials> {
    if !crate::terminal_ui::preparing() {
        command
            .args(["-c", "credential.interactive=false"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "/bin/false")
            .env("SSH_ASKPASS", "/bin/false");
    }
    command.args(["credential", "fill"]);
    let mut request = format!("url={url}\n").into_bytes();
    for challenge in challenges {
        ensure!(
            !challenge
                .iter()
                .any(|byte| matches!(*byte, 0 | b'\r' | b'\n')),
            "invalid Git HTTPS authentication challenge"
        );
        request.extend_from_slice(b"wwwauth[]=");
        request.extend_from_slice(challenge);
        request.push(b'\n');
    }
    request.push(b'\n');
    let scratch = crate::service_storage::Directory::new("credential-")?;
    command.env("TMPDIR", scratch.path());
    let (output, values) =
        crate::git::run_with_output(command, Some(&mut request.as_slice()), |stream| {
            let mut values = Vec::new();
            stream.take(65_537).read_to_end(&mut values)?;
            ensure!(
                values.len() <= 65_536,
                "Git credential response is oversized"
            );
            Ok(values)
        })?;
    ensure!(output.status.success(), "Git HTTPS authentication failed");

    parse_credentials(url, &values)
}

fn parse_credentials(url: &str, values: &[u8]) -> Result<Credentials> {
    use url::Position::{AfterPort, BeforeHost};

    let url = url::Url::parse(url)?;
    let host = url[BeforeHost..AfterPort].as_bytes();
    let decoded_path = crate::git::decode_url_bytes(url.path().trim_start_matches('/').as_bytes());
    let end = decoded_path
        .iter()
        .rposition(|&byte| byte != b'/')
        .map_or(usize::from(!decoded_path.is_empty()), |i| i + 1);
    let path = &decoded_path[..end];
    let mut user = None;
    let mut password = None;
    for line in values
        .split(|&byte| byte == b'\n')
        .take_while(|line| !line.is_empty())
    {
        if let Some(value) = line.strip_prefix(b"username=") {
            user = Some(value);
        } else if let Some(value) = line.strip_prefix(b"password=") {
            password = Some(value);
        } else if let Some(value) = line.strip_prefix(b"protocol=") {
            ensure!(
                value == url.scheme().as_bytes(),
                "Git credential protocol differs from the repository URL"
            );
        } else if let Some(value) = line.strip_prefix(b"host=") {
            ensure!(
                value.eq_ignore_ascii_case(host),
                "Git credential host differs from the repository URL"
            );
        } else if let Some(value) = line.strip_prefix(b"path=") {
            ensure!(
                value == path,
                "Git credential path differs from the repository URL"
            );
        }
    }
    let field = |value: Option<&[u8]>| -> Result<String> {
        let value = value.context("Git HTTPS authentication requires a username and password")?;
        let value =
            std::str::from_utf8(value).context("invalid UTF-8 in Git credential response")?;
        ensure!(!value.contains('\0'), "invalid Git credential response");
        Ok(value.to_owned())
    };
    // The client keeps these credentials on the original repository URL.
    // git credential fill applies the configured host and path lookup.
    Ok(Credentials {
        user: field(user)?,
        password: field(password)?,
    })
}

fn basic_challenge(response: &Easy2<Response>) -> Result<bool> {
    let mut schemes: libc::c_long = 0;
    // Let libcurl parse the challenges it received, including lists within one
    // header. The CURLINFO_LONG result requires a writable C long.
    let result = unsafe {
        curl_sys::curl_easy_getinfo(
            response.raw(),
            curl_sys::CURLINFO_HTTPAUTH_AVAIL,
            &mut schemes as *mut libc::c_long,
        )
    };
    if result != curl_sys::CURLE_OK {
        return Err(curl::Error::new(result).into());
    }
    Ok(schemes as libc::c_ulong & curl_sys::CURLAUTH_BASIC != 0)
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
    Refs(transport::HttpRefs),
    Snapshot(Box<pack_stream::Response<'static>>),
    PendingReply {
        pack: Box<pack_stream::Response<'static>>,
        wake: Arc<UnixStream>,
        report_failure: Option<ReportFailure>,
    },
    Reply(Decoder),
}

struct Decoder {
    input: Option<SyncSender<Vec<u8>>>,
    stop: Arc<AtomicBool>,
    buffered: Arc<AtomicUsize>,
    complete_body: Arc<AtomicBool>,
    report_failure: Option<ReportFailure>,
    terminal_error: Option<String>,
    result: Receiver<Result<pack_stream::Received>>,
    thread: Option<JoinHandle<()>>,
    paused: bool,
}

impl Decoder {
    fn start(
        mut pack: Box<pack_stream::Response<'static>>,
        stop: Arc<AtomicBool>,
        wake: Arc<UnixStream>,
        report_failure: Option<ReportFailure>,
    ) -> io::Result<Self> {
        let (input, chunks) = mpsc::sync_channel::<Vec<u8>>(QUEUE_CHUNKS);
        let (complete, result) = mpsc::channel();
        let buffered = Arc::new(AtomicUsize::new(0));
        let decoder_stop = Arc::new(AtomicBool::new(false));
        pack.set_stop(Some(decoder_stop.clone()));
        let complete_body = Arc::new(AtomicBool::new(false));
        let pending = buffered.clone();
        let body_finished = complete_body.clone();
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
                    if !body_finished.load(Ordering::Acquire) {
                        return Err(OperationCancelled.into());
                    }
                    pack.finish_http()
                })();
                note_failure(report.as_ref(), &received);
                let _ = complete.send(received);
                crate::demand::signal_http2(&wake);
            })?;
        Ok(Self {
            input: Some(input),
            stop: decoder_stop,
            buffered,
            complete_body,
            report_failure,
            terminal_error: None,
            result,
            thread: Some(thread),
            paused: false,
        })
    }

    fn feed(&mut self, data: &[u8]) -> Result<usize, WriteError> {
        if self.buffered.fetch_add(1, Ordering::AcqRel) >= QUEUE_CHUNKS {
            self.buffered.fetch_sub(1, Ordering::AcqRel);
            self.paused = true;
            // Curl will present these same bytes again after resume.
            return Err(WriteError::Pause);
        }
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

    fn end(&mut self, outcome: Result<()>) -> Result<()> {
        note_failure(self.report_failure.as_ref(), &outcome);
        if outcome.is_ok() {
            self.complete_body.store(true, Ordering::Release);
            drop(self.input.take());
        } else {
            if self.report_failure.is_some() {
                self.terminal_error = outcome
                    .as_ref()
                    .err()
                    .filter(|error| !crate::object::is_cancelled(error))
                    .map(|error| format!("{error:#}"));
            }
            self.cancel();
        }
        outcome
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

    fn cancel(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        drop(self.input.take());
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // A publication already in progress can finish after the first report.
        if let (Some(report), Some(error)) = (&self.report_failure, self.terminal_error.take()) {
            report(&anyhow::Error::msg(error));
        }
    }
}

struct Response {
    // The easy handle borrows these bytes until its native cleanup finishes.
    request: Option<Vec<u8>>,
    body: Body,
    error: Option<io::Error>,
    stop: Arc<AtomicBool>,
    authenticated: bool,
    http2: bool,
    ok: bool,
    git_pack: bool,
    challenges: Vec<Vec<u8>>,
}

impl Response {
    fn supports_protocol(&self) -> bool {
        self.http2
    }
}

impl Handler for Response {
    fn write(&mut self, data: &[u8]) -> Result<usize, WriteError> {
        let valid_pack = self.supports_protocol() && self.ok && self.git_pack;
        if valid_pack && matches!(self.body, Body::PendingReply { .. }) {
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
            Body::Refs(refs) if valid_pack => match refs.feed(data) {
                Ok(()) => Ok(data.len()),
                Err(error) => {
                    self.error = Some(io::Error::other(error));
                    Ok(0)
                }
            },
            Body::Snapshot(pack) if valid_pack => match pack.write_http(data) {
                Ok(()) => Ok(data.len()),
                Err(error) => {
                    self.error = Some(io::Error::other(error));
                    Ok(0)
                }
            },
            Body::Reply(decoder) if valid_pack => decoder.feed(data),
            Body::Refs(_) | Body::Snapshot(_) | Body::Reply(_) | Body::PendingReply { .. } => {
                Ok(data.len())
            }
        }
    }

    fn header(&mut self, data: &[u8]) -> bool {
        if data.starts_with(b"HTTP/") {
            self.http2 = data.split(|&byte| byte == b' ').next() == Some(b"HTTP/2");
            self.ok = data.split(u8::is_ascii_whitespace).nth(1) == Some(b"200");
            self.git_pack = false;
            self.challenges.clear();
            match &mut self.body {
                Body::Discovery(bytes) => bytes.clear(),
                Body::Refs(refs) => refs.reset(),
                _ => {}
            }
        } else if let Some(colon) = data.iter().position(|&byte| byte == b':') {
            if data[..colon].eq_ignore_ascii_case(b"content-type") {
                self.git_pack = content_type_is(Some(&data[colon + 1..]), PACK_RESULT);
            }
            if data[..colon].eq_ignore_ascii_case(b"www-authenticate") {
                self.challenges
                    .push(data[colon + 1..].trim_ascii().to_vec());
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
    user_agent: String,
    auth: Option<Credentials>,
    credential: Option<CredentialProvider>,
    multi: Multi,
    active: HashMap<usize, Easy2Handle<Response>>,
    decoding: HashMap<usize, Decoder>,
    retiring: Vec<Decoder>,
    stop: Arc<AtomicBool>,
}

pub(crate) enum Selection {
    Exact,
    Ref(transport::RefQuery),
}

impl Drop for Client {
    fn drop(&mut self) {
        for handle in self.active.values_mut() {
            if let Body::Reply(decoder) = &mut handle.get_mut().body {
                decoder.cancel();
            }
        }
        for decoder in self.decoding.values_mut().chain(&mut self.retiring) {
            decoder.cancel();
        }
    }
}

impl Client {
    pub(crate) fn negotiate(
        config: Configuration,
        stop: Arc<AtomicBool>,
        selection: Selection,
        mode: transport::FetchMode,
    ) -> Result<(Self, Option<Oid>)> {
        let mut multi = Multi::new();
        multi.pipelining(false, true)?;
        // All submitted requests share the connection. libcurl applies the
        // peer's HTTP/2 stream and flow-control limits.
        multi.set_max_host_connections(1)?;
        let mut client = Self {
            base: config.base,
            ca: config.ca,
            user_agent: config.user_agent,
            auth: None,
            credential: Some(config.credential),
            multi,
            active: HashMap::new(),
            decoding: HashMap::new(),
            retiring: Vec::new(),
            stop,
        };
        let easy = client.easy(
            "info/refs?service=git-upload-pack",
            Body::Discovery(Vec::new()),
            None,
        )?;
        let response = client.perform_with_authentication(easy)?;
        ensure!(
            response.get_ref().supports_protocol(),
            "Git HTTPS requires HTTP/2"
        );
        let status = response.response_code()?;
        ensure!(
            status == 200,
            "Git HTTPS advertisement returned HTTP {status}"
        );
        ensure!(
            content_type_is(response.content_type_bytes()?, ADVERTISEMENT),
            "expected a smart Git HTTPS advertisement"
        );
        let Body::Discovery(bytes) = &response.get_ref().body else {
            unreachable!()
        };
        ensure!(
            transport::read_http_capabilities(bytes, mode)?,
            "server must support Git protocol v2 and the requested fetch capabilities"
        );
        let selected = match selection {
            Selection::Exact => None,
            Selection::Ref(reference) => Some(client.query_ref(reference)?),
        };
        Ok((client, selected))
    }

    fn query_ref(&mut self, reference: transport::RefQuery) -> Result<Oid> {
        let request = transport::reference_request(&reference)?;
        let easy = self.easy(
            "git-upload-pack",
            Body::Refs(transport::HttpRefs::new(reference)),
            Some(request),
        )?;
        let response = self.perform_with_authentication(easy)?;
        let status = response.response_code()?;
        ensure!(
            status == 200,
            "Git HTTPS ref selection returned HTTP {status}"
        );
        ensure!(
            response.get_ref().supports_protocol()
                && content_type_is(response.content_type_bytes()?, PACK_RESULT),
            "invalid Git HTTP/2 ref reply"
        );
        let Body::Refs(refs) = &response.get_ref().body else {
            unreachable!()
        };
        refs.finish()?
            .context("remote ref did not select one object; use a full Git ref when names compete")
    }

    fn easy(&self, service: &str, body: Body, request: Option<Vec<u8>>) -> Result<Easy2<Response>> {
        let post = request.is_some();
        let mut easy = Easy2::new(Response {
            request,
            body,
            error: None,
            stop: self.stop.clone(),
            authenticated: false,
            http2: false,
            ok: false,
            git_pack: false,
            challenges: Vec::new(),
        });
        easy.url(&format!("{}/{service}", self.base))?;
        easy.proxy("")?;
        easy.useragent(&self.user_agent)?;
        easy.http_version(HttpVersion::V2TLS)?;
        easy.netrc(NetRc::Optional)?;
        easy.http_auth(Auth::new().basic(true))?;
        easy.accept_encoding("")?;
        easy.pipewait(true)?;
        easy.progress(true)?;
        if let Some(ca) = &self.ca {
            easy.cainfo(ca)?;
        }
        if let Some((length, bytes)) = easy
            .get_ref()
            .request
            .as_ref()
            .map(|request| (request.len(), request.as_ptr().cast::<libc::c_char>()))
        {
            easy.post_field_size(length as u64)?;
            // Safety: CURLOPT_POSTFIELDS borrows exactly the number of bytes
            // set above. The handler owns the allocation, never mutates or
            // replaces it, and Easy2 cleans up CURL before dropping its handler.
            let code = unsafe {
                curl_sys::curl_easy_setopt(easy.raw(), curl_sys::CURLOPT_POSTFIELDS, bytes)
            };
            if code != curl_sys::CURLE_OK {
                let mut error = curl::Error::new(code);
                if let Some(detail) = easy.take_error_buf() {
                    error.set_extra(detail);
                }
                return Err(error.into());
            }
        }
        self.apply_request_headers(&mut easy, post)?;
        Ok(easy)
    }

    fn apply_request_headers(&self, easy: &mut Easy2<Response>, post: bool) -> Result<()> {
        let mut headers = List::new();
        headers.append("Git-Protocol: version=2")?;
        if post {
            headers.append("Content-Type: application/x-git-upload-pack-request")?;
            headers.append("Accept: application/x-git-upload-pack-result")?;
        }
        if let Some(auth) = &self.auth {
            easy.netrc(NetRc::Ignored)?;
            easy.username(&auth.user)?;
            easy.password(&auth.password)?;
            easy.get_mut().authenticated = true;
        }
        easy.http_headers(headers)?;
        Ok(())
    }

    fn perform_with_authentication(&mut self, easy: Easy2<Response>) -> Result<Easy2<Response>> {
        let post = easy.get_ref().request.is_some();
        let mut response = self.perform(easy)?;
        if self.authenticate(&response)? {
            self.apply_request_headers(&mut response, post)?;
            response = self.perform(response)?;
        }
        Self::check_authentication(&response)?;
        Ok(response)
    }

    fn check_authentication(response: &Easy2<Response>) -> Result<()> {
        ensure!(
            response.response_code()? != 401,
            "Git HTTPS authentication failed"
        );
        Ok(())
    }

    fn authenticate(&mut self, response: &Easy2<Response>) -> Result<bool> {
        let headers = response.get_ref();
        if response.response_code()? != 401
            || !headers.supports_protocol()
            || headers.authenticated
            || !basic_challenge(response)?
        {
            return Ok(false);
        }
        if let Some(fetch) = self.credential.take() {
            self.auth = Some(fetch(&self.base, &headers.challenges)?);
        }
        // A concurrent request can receive its challenge after another
        // request already supplied the session credential.
        Ok(self.auth.is_some())
    }

    fn perform(&self, easy: Easy2<Response>) -> Result<Easy2<Response>> {
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
            ensure!(running != 0, "Git HTTPS request ended without a result");
            self.multi.wait(&mut [], Duration::from_millis(100))?;
        }
    }

    pub(crate) fn fetch_snapshot(
        &mut self,
        oid: Oid,
        response: pack_stream::Response<'static>,
    ) -> Result<pack_stream::Received> {
        let request = transport::fetch_request(&[oid], transport::FetchMode::Snapshot)?;
        let easy = self.easy(
            "git-upload-pack",
            Body::Snapshot(Box::new(response)),
            Some(request),
        )?;
        let mut response = self.perform_with_authentication(easy)?;
        self.check_response(&mut response, Ok(()))?;
        match std::mem::replace(&mut response.get_mut().body, Body::Discovery(Vec::new())) {
            Body::Snapshot(pack) => pack.finish_http(),
            _ => unreachable!("expected eager pack response"),
        }
    }

    pub(crate) fn start(
        &mut self,
        token: usize,
        oids: &[Oid],
        response: pack_stream::Response<'static>,
        wake: &Arc<UnixStream>,
        report_failure: Option<ReportFailure>,
    ) -> Result<()> {
        let request = transport::fetch_request(oids, transport::FetchMode::Filtered)?;
        let easy = self.easy(
            "git-upload-pack",
            Body::PendingReply {
                pack: Box::new(response),
                wake: wake.clone(),
                report_failure,
            },
            Some(request),
        )?;
        self.submit(token, easy)
    }

    fn submit(&mut self, token: usize, easy: Easy2<Response>) -> Result<()> {
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
        self.retiring.retain(|decoder| decoder.result().is_none());
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
        if outcome.is_ok()
            && !self.stop.load(Ordering::Relaxed)
            && check_transfer().is_ok()
            && matches!(&response.get_ref().body, Body::PendingReply { .. })
        {
            match self.authenticate(&response) {
                Ok(true) => {
                    let retry = self
                        .apply_request_headers(&mut response, true)
                        .and_then(|()| self.submit(token, response));
                    if let Err(error) = retry {
                        finished.push((token, Err(error)));
                    }
                    return Ok(());
                }
                Ok(false) => {}
                Err(error) => {
                    finished.push((token, Err(error)));
                    return Ok(());
                }
            }
        }
        let network = self.check_response(&mut response, outcome);
        let body = std::mem::replace(&mut response.get_mut().body, Body::Discovery(Vec::new()));
        match body {
            Body::Reply(mut decoder) => match decoder.end(network) {
                Ok(()) => {
                    self.decoding.insert(token, decoder);
                }
                Err(error) => {
                    // Report the terminal error before retiring the worker. A
                    // local cache spill can still be finishing in this decoder.
                    let result = decoder.result().filter(|result| {
                        result
                            .as_ref()
                            .is_err_and(|error| !crate::object::is_cancelled(error))
                    });
                    finished.push((token, result.unwrap_or(Err(error))));
                    self.retiring.push(decoder);
                }
            },
            Body::PendingReply { pack, .. } => {
                finished.push((token, network.and_then(|()| pack.finish_http())));
            }
            Body::Refs(_) | Body::Discovery(_) | Body::Snapshot(_) => {
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
        Self::check_authentication(response)?;
        ensure!(status == 200, "Git HTTP/2 fetch returned HTTP {status}");
        ensure!(
            response.get_ref().supports_protocol()
                && content_type_is(response.content_type_bytes()?, PACK_RESULT),
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
    use std::{
        io::Write,
        sync::{mpsc, Mutex},
        thread,
    };

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
    fn credential_scope_matches_native_git_url_components() -> Result<()> {
        let url = "https://example.com:8443/project%20name/repo.git/";
        let response = b"protocol=https\nhost=EXAMPLE.com:8443\npath=project name/repo.git\nusername=fixture\npassword=value\n";
        let credentials = parse_credentials(url, response)?;
        assert_eq!(credentials.user, "fixture");
        assert!(
            parse_credentials("https://example.com:8444/project%20name/repo.git", response)
                .is_err()
        );
        assert!(parse_credentials("https://example.com:8443/another.git", response).is_err());
        Ok(())
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
    fn terminal_transport_failure_reaches_the_reader_before_the_decoder_retires() {
        let (oid, frame, _) = framed_blob();
        let directory = tempfile::tempdir().unwrap();
        let scratch = crate::raw_blob::RawBlobs::new(directory.path()).unwrap();
        let (publish, published) = mpsc::channel();
        let (release, resumed) = mpsc::channel();
        let exposed = Arc::new(AtomicBool::new(false));
        let expose = exposed.clone();
        let pack = pack_stream::Response::new(
            scratch,
            vec![oid],
            Box::new(move |_| {
                publish.send(())?;
                resumed.recv_timeout(Duration::from_secs(3))?;
                expose.store(true, Ordering::Release);
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
                let _ = report.send((error.to_string(), exposed.load(Ordering::Acquire)));
            })),
        )
        .unwrap();
        decoder.feed(&frame).unwrap();
        published.recv_timeout(Duration::from_secs(3)).unwrap();
        let multi = Multi::new();
        let handle = multi
            .add2(Easy2::new(Response {
                request: None,
                body: Body::Reply(decoder),
                error: Some(io::Error::other("required transfer was reset")),
                stop: stop.clone(),
                authenticated: false,
                http2: true,
                ok: true,
                git_pack: true,
                challenges: Vec::new(),
            }))
            .unwrap();
        let mut client = Client {
            base: String::new(),
            ca: None,
            user_agent: String::new(),
            auth: None,
            credential: None,
            multi,
            active: HashMap::from([(1, handle)]),
            decoding: HashMap::new(),
            retiring: Vec::new(),
            stop,
        };
        let mut finished = Vec::new();
        let result = client.end_transfer(1, Ok(()), &mut finished);
        release.send(()).unwrap();
        result.unwrap();
        let (token, result) = finished
            .pop()
            .expect("transport failure waited for local publication");
        assert_eq!(token, 1);
        assert_eq!(
            result.err().unwrap().to_string(),
            "required transfer was reset"
        );
        let expected = "required transfer was reset".to_owned();
        assert_eq!(reported.try_recv().unwrap(), (expected.clone(), false));
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !client.retiring.is_empty() {
            assert!(client.progress().unwrap().is_empty());
            assert!(std::time::Instant::now() < deadline);
            thread::yield_now();
        }
        assert_eq!(reported.try_recv().unwrap(), (expected, true));
    }

    #[test]
    fn a_pack_buffer_wait_ends_on_reset_or_drop() {
        for failure in [None, Some("required transfer was reset")] {
            let directory = tempfile::tempdir().unwrap();
            let scratch = crate::raw_blob::RawBlobs::new(directory.path()).unwrap();
            let occupied: Vec<_> = (0..8)
                .map(|_| {
                    scratch
                        .reserve_pack_decode(
                            || crate::raw_blob::PackDecodePriority::Demand,
                            check_transfer,
                        )
                        .unwrap()
                })
                .collect();
            let wait_for_slot_wait = scratch.pack_decode_wait_notice();
            let pack =
                pack_stream::Response::new(scratch.clone(), vec![], Box::new(|_| unreachable!()));
            let (_read, wake) = UnixStream::pair().unwrap();
            let mut decoder =
                Decoder::start(Box::new(pack), Arc::default(), Arc::new(wake), None).unwrap();
            // A pack header and the entry header for a 65537-byte blob.
            decoder
                .feed(b"000dpackfile\n0014\x01PACK\0\0\0\x02\0\0\0\x01\xb1\x80\x20")
                .unwrap();
            // The sender owns the slot mutex until it enters the condvar wait.
            let active_when_waiting = wait_for_slot_wait
                .recv_timeout(Duration::from_secs(3))
                .map(|()| scratch.active_pack_decodes());
            let (finished, wait_for_finish) = mpsc::channel();
            let join = thread::spawn(move || {
                if let Some(failure) = failure {
                    let failure = decoder.end(Err(anyhow::anyhow!(failure))).unwrap_err();
                    let result = decoder.result.recv().unwrap();
                    assert!(result.as_ref().is_err_and(crate::object::is_cancelled));
                    finished.send(Some(failure.to_string())).unwrap();
                } else {
                    drop(decoder);
                    finished.send(None).unwrap();
                }
            });
            let cancelled = wait_for_finish.recv_timeout(Duration::from_secs(1));
            let permits = occupied.len();
            drop(occupied);
            join.join().unwrap();
            assert_eq!(
                active_when_waiting.expect("decoder did not wait for a slot"),
                permits
            );
            assert_eq!(
                cancelled
                    .expect("decoder waited for a different stream's buffer")
                    .as_deref(),
                failure
            );
        }
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
            request: None,
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
            authenticated: false,
            http2: true,
            ok: true,
            git_pack: true,
            challenges: Vec::new(),
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
                decoder.end(Ok(())).unwrap();
                let received = decoder
                    .result
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                assert_ne!(received.bytes, 0);
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
