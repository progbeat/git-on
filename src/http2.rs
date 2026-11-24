//! Git protocol v2 exact-object requests multiplexed by an invocation-owned
//! libcurl multi handle. Native Git still selects and verifies the revision.
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
    ops::Range,
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

type CredentialProvider =
    Box<dyn FnMut(&str, &[Vec<u8>], Option<&Credentials>) -> Result<Option<Credentials>> + Send>;

#[derive(Debug)]
pub(crate) struct AuthenticationRejected;

impl std::fmt::Display for AuthenticationRejected {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str("Git HTTPS authentication failed")
    }
}

impl std::error::Error for AuthenticationRejected {}

#[derive(Debug)]
pub(crate) struct NativeAuthentication;

impl std::fmt::Display for NativeAuthentication {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str("Git HTTPS request requires native authentication")
    }
}

impl std::error::Error for NativeAuthentication {}

#[derive(Debug)]
pub(crate) struct Http1Required;

impl std::fmt::Display for Http1Required {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str("Git upload-pack switched to HTTP/1.x")
    }
}

impl std::error::Error for Http1Required {}

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
        credential: impl FnMut(&str, &[Vec<u8>], Option<&Credentials>) -> Result<Option<Credentials>>
            + Send
            + 'static,
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
            // Older defaults also send custom origin headers to CONNECT proxies.
            || version.version_num() < 0x07_2a_01
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
    response: Vec<u8>,
    authorization: Authorization,
    continuing: bool,
}

enum Authorization {
    Password {
        user: Range<usize>,
        password: Range<usize>,
    },
    Encoded {
        scheme: Range<usize>,
        value: Range<usize>,
    },
}

impl Credentials {
    pub(crate) fn continues_encoded_authentication(&self) -> bool {
        self.continuing && matches!(self.authorization, Authorization::Encoded { .. })
    }
}

fn append_authentication_challenges(request: &mut Vec<u8>, challenges: &[Vec<u8>]) -> bool {
    let mut valid = true;
    for challenge in challenges {
        if challenge
            .iter()
            .any(|byte| matches!(*byte, 0 | b'\r' | b'\n'))
        {
            valid = false;
            continue;
        }
        request.extend_from_slice(b"wwwauth[]=");
        request.extend_from_slice(challenge);
        request.push(b'\n');
    }
    valid
}

pub(crate) fn credentials(
    mut command: Command,
    url: &str,
    challenges: &[Vec<u8>],
    previous: Option<&Credentials>,
    scratch: &mut Option<crate::service_storage::Directory>,
) -> Result<Option<Credentials>> {
    command
        .args(["-c", "credential.interactive=false", "credential", "fill"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "/bin/false")
        .env("SSH_ASKPASS", "/bin/false");
    // Git's URL parser clears credential fields it has already read.
    let mut request =
        format!("url={url}\ncapability[]=authtype\ncapability[]=state\n").into_bytes();
    if let Some(previous) = previous {
        // Match Git's next credential fill after it clears the old secrets.
        for line in previous.response.split(|&byte| byte == b'\n') {
            if [
                b"username=".as_slice(),
                b"state[]=",
                b"oauth_refresh_token=",
                b"authtype=",
                b"ephemeral=",
                b"continue=",
            ]
            .iter()
            .any(|prefix| line.starts_with(prefix))
            {
                request.extend_from_slice(line);
                request.push(b'\n');
            }
        }
    }
    if !append_authentication_challenges(&mut request, challenges) {
        return Ok(None);
    }
    request.push(b'\n');
    let scratch = match scratch {
        Some(directory) => directory,
        None => scratch.insert(crate::service_storage::Directory::new("credential-")?),
    };
    command.env("TMPDIR", scratch.path());
    let (output, mut values) =
        crate::git::run_with_output(command, Some(&mut request.as_slice()), |stream| {
            let mut values = Vec::new();
            stream.take(65_537).read_to_end(&mut values)?;
            ensure!(
                values.len() <= 65_536,
                "Git credential response is oversized"
            );
            Ok(values)
        })?;
    if !output.status.success() {
        // Git uses the same exit code for a missing credential and a
        // helper's explicit quit. Its quit diagnostic is untranslated and
        // can be followed by trace output.
        let helper = b"credential helper '";
        if output.status.code() == Some(128)
            && output
                .stderr
                .windows(helper.len())
                .any(|part| part == helper)
            && output
                .stderr
                .split(|&byte| byte == b'\n')
                .any(|line| line.trim_ascii_end().ends_with(b"' told us to quit"))
        {
            return Err(AuthenticationRejected.into());
        }
        return Ok(None);
    }
    discard_returned_challenges(&mut values);
    let Some((scheme, authority)) = url.split_once("://") else {
        return Ok(None);
    };
    let expected = authority.split('/').next().unwrap_or_default();
    let mut protocol = None;
    let mut host = None;
    let mut continuing = false;
    for line in values.split(|&byte| byte == b'\n') {
        if let Some(value) = line.strip_prefix(b"protocol=") {
            protocol = Some(value);
        } else if let Some(value) = line.strip_prefix(b"host=") {
            host = Some(value);
        } else if line == b"continue=1" {
            continuing = true;
        } else if line.starts_with(b"path=") {
            // The reusable client uses host-scoped credentials.
            return Ok(None);
        }
    }
    if protocol != Some(scheme.as_bytes()) || host != Some(expected.as_bytes()) {
        return Ok(None);
    }
    let mut fields = [None, None, None, None];
    let mut offset = 0;
    for record in values.split_inclusive(|&byte| byte == b'\n') {
        let line = record.strip_suffix(b"\n").unwrap_or(record);
        for (slot, prefix) in fields.iter_mut().zip([
            b"username=".as_slice(),
            b"password=",
            b"authtype=",
            b"credential=",
        ]) {
            if line.starts_with(prefix) {
                *slot = Some(offset + prefix.len()..offset + line.len());
            }
        }
        offset += record.len();
    }
    let authorization = match fields {
        [_, _, Some(scheme), Some(value)] => {
            let scheme_bytes = &values[scheme.clone()];
            let credential = &values[value.clone()];
            if scheme_bytes.is_empty()
                || auth_token_len(scheme_bytes) != scheme_bytes.len()
                || credential.is_empty()
                || !credential
                    .iter()
                    .all(|byte| matches!(byte, b'\t' | b' '..=b'~'))
            {
                return Ok(None);
            }
            Authorization::Encoded { scheme, value }
        }
        [Some(user), Some(password), _, None]
            if [user.clone(), password.clone()].iter().all(|range| {
                std::str::from_utf8(&values[range.clone()]).is_ok_and(|value| !value.contains('\0'))
            }) =>
        {
            Authorization::Password { user, password }
        }
        _ => return Ok(None),
    };
    Ok(Some(Credentials {
        response: values,
        authorization,
        continuing,
    }))
}

fn discard_returned_challenges(response: &mut Vec<u8>) {
    let mut read = 0;
    let mut kept = 0;
    while read < response.len() {
        let end = response[read..]
            .iter()
            .position(|&byte| byte == b'\n')
            .map_or(response.len(), |length| read + length + 1);
        if !response[read..end].starts_with(b"wwwauth[]=") {
            if kept != read {
                response.copy_within(read..end, kept);
            }
            kept += end - read;
        }
        read = end;
    }
    response.truncate(kept);
}

pub(crate) fn replayable_credential_challenge(
    challenges: &[Vec<u8>],
    encoded_continuation: bool,
) -> bool {
    // Encoded credentials let Git's helper supply the entire header for
    // any subsequent challenge in the same already-started exchange.
    replayable_basic_challenge(challenges) || (encoded_continuation && !challenges.is_empty())
}

fn replayable_basic_challenge(challenges: &[Vec<u8>]) -> bool {
    // Several realms can offer the same scheme. The helper sees all the
    // challenges and can continue its exchange without starting over.
    !challenges.is_empty()
        && challenges.iter().all(|challenge| {
            let mut fields = challenge.splitn(2, u8::is_ascii_whitespace);
            fields
                .next()
                .is_some_and(|scheme| scheme.eq_ignore_ascii_case(b"basic"))
                && fields.next().and_then(basic_parameters).is_some()
        })
}

fn basic_parameters(mut parameters: &[u8]) -> Option<()> {
    let mut realm = false;
    loop {
        let parameter = parameters.trim_ascii_start();
        let (name, remainder) = parameter.split_at(auth_token_len(parameter));
        if name.is_empty() {
            return None;
        }
        let value = remainder
            .trim_ascii_start()
            .strip_prefix(b"=")?
            .trim_ascii_start();
        let tail = if let Some(quoted) = value.strip_prefix(b"\"") {
            let mut escaped = false;
            let end = quoted.iter().position(|&byte| {
                let closing_quote = !escaped && byte == b'"';
                escaped = !escaped && byte == b'\\';
                closing_quote
            })?;
            &quoted[end + 1..]
        } else {
            let len = auth_token_len(value);
            if len == 0 {
                return None;
            }
            &value[len..]
        };
        if name.eq_ignore_ascii_case(b"realm") {
            if realm {
                return None;
            }
            realm = true;
        }
        let tail = tail.trim_ascii_start();
        if tail.is_empty() {
            return realm.then_some(());
        }
        // A comma can introduce another scheme. Requiring a parameter at
        // the next iteration leaves such mixed exchanges to native Git.
        parameters = tail.strip_prefix(b",")?;
    }
}

fn auth_token_len(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .position(|byte| !byte.is_ascii_alphanumeric() && !b"!#$%&'*+-.^_\x60|~".contains(byte))
        .unwrap_or(bytes.len())
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum HttpProtocol {
    Http1,
    Http2,
}

enum FetchPreparation {
    NativeHttp1,
    ProbeAuthentication,
}

#[derive(Clone, Copy)]
enum InitialFetch {
    Exact,
    Branch,
    AfterRefProbe,
}

struct Response {
    // The easy handle borrows these bytes until its native cleanup finishes.
    request: Option<Vec<u8>>,
    body: Body,
    error: Option<io::Error>,
    stop: Arc<AtomicBool>,
    authentication: Option<u64>,
    encoded_continuation: bool,
    protocol: Option<HttpProtocol>,
    headers_complete: bool,
    initial_fetch: Option<InitialFetch>,
    allow_http1: bool,
    ok: bool,
    git_pack: bool,
    challenges: Vec<Vec<u8>>,
}

impl Response {
    fn supports_protocol(&self) -> bool {
        match self.protocol {
            Some(HttpProtocol::Http2) => true,
            Some(HttpProtocol::Http1) => self.allow_http1,
            None => false,
        }
    }

    fn fetch_preparation(
        &self,
        response_code: impl FnOnce() -> Result<u32, curl::Error>,
    ) -> Option<FetchPreparation> {
        if !self.headers_complete
            || !matches!(self.body, Body::PendingReply { .. } | Body::Snapshot(_))
        {
            return None;
        }
        let initial = self.initial_fetch?;
        let downgraded = !self.allow_http1 && self.protocol == Some(HttpProtocol::Http1);
        let can_probe_auth = matches!(initial, InitialFetch::Exact);
        if !downgraded && !can_probe_auth {
            return None;
        }
        match response_code().ok()? {
            200..=599 if downgraded => Some(FetchPreparation::NativeHttp1),
            200 if self.git_pack && self.supports_protocol() => None,
            401 | 407 => None,
            // Some endpoints challenge ref queries after refusing a fetch.
            // Retry with the original ref probe after a non-pack reply.
            200..=599 if can_probe_auth => Some(FetchPreparation::ProbeAuthentication),
            _ => None,
        }
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
            Body::Snapshot(pack) if valid_pack => match pack.write_http(data) {
                Ok(()) => Ok(data.len()),
                Err(error) => {
                    self.error = Some(io::Error::other(error));
                    Ok(0)
                }
            },
            Body::Reply(decoder) if valid_pack => decoder.feed(data),
            Body::Snapshot(_) | Body::Reply(_) | Body::PendingReply { .. } => Ok(data.len()),
        }
    }

    fn header(&mut self, data: &[u8]) -> bool {
        if data.starts_with(b"HTTP/") {
            self.protocol = match data.split(|&byte| byte == b' ').next() {
                Some(b"HTTP/2") => Some(HttpProtocol::Http2),
                Some(b"HTTP/1.0" | b"HTTP/1.1") => Some(HttpProtocol::Http1),
                _ => None,
            };
            self.ok = data.split(u8::is_ascii_whitespace).nth(1) == Some(b"200");
            self.git_pack = false;
            self.headers_complete = false;
            self.challenges.clear();
            if let Body::Discovery(bytes) = &mut self.body {
                bytes.clear();
            }
        } else if matches!(data, b"\r\n" | b"\n") {
            self.headers_complete = true;
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
    auth_generation: u64,
    credential: Option<CredentialProvider>,
    allow_http1: bool,
    initial_fetch: Option<InitialFetch>,
    multi: Multi,
    active: HashMap<usize, Easy2Handle<Response>>,
    decoding: HashMap<usize, Decoder>,
    retiring: Vec<Decoder>,
    stop: Arc<AtomicBool>,
}

pub(crate) enum Selection {
    Exact,
    Branch(transport::BranchQuery),
    Demand,
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
    ) -> Result<Option<(Self, Option<git2::Oid>)>> {
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
            auth_generation: 0,
            credential: Some(config.credential),
            allow_http1: matches!(mode, transport::FetchMode::Snapshot { .. }),
            initial_fetch: match &selection {
                Selection::Exact => Some(InitialFetch::Exact),
                Selection::Branch(_) => Some(InitialFetch::Branch),
                Selection::Demand => None,
            },
            multi,
            active: HashMap::new(),
            decoding: HashMap::new(),
            retiring: Vec::new(),
            stop,
        };
        let response = client.discovery("info/refs?service=git-upload-pack", None)?;
        if response.response_code()? != 200
            || !response.get_ref().supports_protocol()
            || !content_type_is(response.content_type_bytes()?, ADVERTISEMENT)
        {
            return Ok(None);
        }
        let Body::Discovery(bytes) = &response.get_ref().body else {
            unreachable!()
        };
        if !transport::read_http_capabilities(bytes, mode)? {
            return Ok(None);
        }
        let branch = match &selection {
            // An exact OID only needs the capability advertisement. The
            // first fetch can authenticate before decoding any pack bytes.
            Selection::Exact => return Ok(Some((client, None))),
            Selection::Branch(branch) => Some(branch),
            // A pool opened after native selection checks POST access
            // before it takes over reads of the already mounted snapshot.
            Selection::Demand => None,
        };
        Ok(client
            .query_refs(branch)?
            .map(|selected| (client, selected)))
    }

    fn query_refs(
        &mut self,
        branch: Option<&transport::BranchQuery>,
    ) -> Result<Option<Option<Oid>>> {
        let response = self.discovery(
            "git-upload-pack",
            Some(transport::reference_request(branch)?),
        )?;
        if response.response_code()? != 200
            || !response.get_ref().supports_protocol()
            || !content_type_is(response.content_type_bytes()?, PACK_RESULT)
        {
            return Ok(None);
        }
        let Body::Discovery(bytes) = &response.get_ref().body else {
            unreachable!()
        };
        let selected = transport::read_http_branch(bytes, branch)?;
        Ok((branch.is_none() || selected.is_some()).then_some(selected))
    }

    fn easy(&self, service: &str, body: Body, request: Option<Vec<u8>>) -> Result<Easy2<Response>> {
        let post = request.is_some();
        let mut easy = Easy2::new(Response {
            request,
            body,
            error: None,
            stop: self.stop.clone(),
            authentication: None,
            encoded_continuation: false,
            protocol: None,
            headers_complete: false,
            initial_fetch: None,
            allow_http1: self.allow_http1,
            ok: false,
            git_pack: false,
            challenges: Vec::new(),
        });
        easy.url(&format!("{}/{service}", self.base))?;
        easy.useragent(&self.user_agent)?;
        easy.http_version(HttpVersion::V2TLS)?;
        easy.netrc(NetRc::Optional)?;
        easy.http_auth(Auth::new().auto(true))?;
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
            let field = |range: &Range<usize>| std::str::from_utf8(&auth.response[range.clone()]);
            match &auth.authorization {
                Authorization::Password { user, password } => {
                    easy.netrc(NetRc::Optional)?;
                    easy.username(field(user)?)?;
                    easy.password(field(password)?)?;
                    easy.http_auth(Auth::new().basic(true))?;
                }
                Authorization::Encoded { scheme, value } => {
                    // A complete header supersedes libcurl's own credentials.
                    easy.netrc(NetRc::Ignored)?;
                    easy.http_auth(&Auth::new())?;
                    headers.append(&format!(
                        "Authorization: {} {}",
                        field(scheme)?,
                        field(value)?
                    ))?;
                }
            }
            let response = easy.get_mut();
            response.authentication = Some(self.auth_generation);
            response.encoded_continuation = auth.continues_encoded_authentication();
        }
        easy.http_headers(headers)?;
        Ok(())
    }

    fn discovery(&mut self, service: &str, request: Option<Vec<u8>>) -> Result<Easy2<Response>> {
        let easy = self.easy(service, Body::Discovery(Vec::new()), request)?;
        self.perform_with_authentication(easy)
    }

    fn perform_with_authentication(&mut self, easy: Easy2<Response>) -> Result<Easy2<Response>> {
        let post = easy.get_ref().request.is_some();
        let mut response = self.perform(easy)?;
        while self.authenticate(&response)? {
            self.apply_request_headers(&mut response, post)?;
            response = self.perform(response)?;
        }
        self.check_authentication(&response)?;
        Ok(response)
    }

    fn check_authentication(&self, response: &Easy2<Response>) -> Result<()> {
        if response.get_ref().authentication != Some(self.auth_generation)
            || response.response_code()? != 401
        {
            return Ok(());
        }
        match self.auth.as_ref() {
            Some(auth) if auth.continuing => Err(NativeAuthentication.into()),
            Some(_) => Err(AuthenticationRejected.into()),
            None => Ok(()),
        }
    }

    fn authenticate(&mut self, response: &Easy2<Response>) -> Result<bool> {
        self.authenticate_response(response.response_code()?, response.get_ref())
    }

    fn authenticate_response(&mut self, code: u32, headers: &Response) -> Result<bool> {
        if code != 401
            || !headers.supports_protocol()
            || !replayable_credential_challenge(&headers.challenges, headers.encoded_continuation)
        {
            return Ok(false);
        }
        if let Some(auth) = &self.auth {
            if headers.authentication != Some(self.auth_generation) {
                // Other in-flight requests can finish after a credential
                // transition. Replay them with the new credential state.
                return Ok(true);
            }
            if !auth.continuing {
                return Ok(false);
            }
        }
        if let Some(mut fetch) = self.credential.take() {
            self.auth = fetch(&self.base, &headers.challenges, self.auth.as_ref())?;
            if let Some(auth) = &self.auth {
                self.auth_generation += 1;
                if auth.continuing {
                    self.credential = Some(fetch);
                }
                return Ok(true);
            }
        }
        Ok(false)
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
    ) -> Result<Option<(pack_stream::Received, bool)>> {
        // Eager preparation has one transfer. Decode directly from curl's
        // callback buffer instead of queueing immutable chunks for a worker.
        let request = transport::fetch_request(
            &[oid],
            transport::FetchMode::Snapshot {
                selecting_ref: false,
            },
        )?;
        let mut easy = self.easy(
            "git-upload-pack",
            Body::Snapshot(Box::new(response)),
            Some(request),
        )?;
        easy.get_mut().initial_fetch = self.initial_fetch.take();
        let mut response = self.perform_with_authentication(easy)?;
        if let Some(FetchPreparation::ProbeAuthentication) = response
            .get_ref()
            .fetch_preparation(|| response.response_code())
        {
            self.probe_exact_fetch_refs()?;
            self.apply_request_headers(&mut response, true)?;
            response = self.perform_with_authentication(response)?;
        }
        if matches!(response.response_code()?, 401 | 407) {
            return Ok(None);
        }
        self.check_response(&mut response, Ok(()))?;
        let http2 = response.get_ref().protocol == Some(HttpProtocol::Http2);
        match std::mem::replace(&mut response.get_mut().body, Body::Discovery(Vec::new())) {
            Body::Snapshot(pack) => pack.finish_http().map(|received| Some((received, http2))),
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
        let mut easy = self.easy(
            "git-upload-pack",
            Body::PendingReply {
                pack: Box::new(response),
                wake: wake.clone(),
                report_failure,
            },
            Some(request),
        )?;
        easy.get_mut().initial_fetch = self.initial_fetch.take();
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

        // The completed origin headers can require another preparation step
        // while the server is still sending its body. CURLINFO_RESPONSE_CODE
        // excludes the CONNECT response of an HTTPS proxy.
        if !self.stop.load(Ordering::Relaxed) && check_transfer().is_ok() {
            let fallbacks: Vec<_> = self
                .active
                .iter()
                .filter_map(|(&token, handle)| {
                    let preparation = handle
                        .get_ref()
                        .fetch_preparation(|| handle.response_code())?;
                    Some((token, preparation))
                })
                .collect();
            for (token, preparation) in fallbacks {
                let handle = self
                    .active
                    .remove(&token)
                    .expect("active Git HTTP/2 request");
                let response = self.multi.remove2(handle)?;
                if let Err(error) = self.complete_preparation(token, response, preparation) {
                    finished.push((token, Err(error)));
                }
            }
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
        if !self.stop.load(Ordering::Relaxed)
            && check_transfer().is_ok()
            && matches!(&response.get_ref().body, Body::PendingReply { .. })
        {
            if let Some(preparation) = response
                .get_ref()
                .fetch_preparation(|| response.response_code())
            {
                if let Err(error) = self.complete_preparation(token, response, preparation) {
                    finished.push((token, Err(error)));
                }
                return Ok(());
            }
            if outcome.is_ok()
                && (response.get_ref().authentication != Some(self.auth_generation)
                    || self.auth.as_ref().is_some_and(|auth| auth.continuing))
                && matches!(response.response_code()?, 401 | 407)
            {
                // A challenge before any pack bytes reached the decoder can
                // replay the same curl request with the auth options filled in.
                if let Err(error) = self.retry_authentication(token, response) {
                    finished.push((token, Err(error)));
                }
                return Ok(());
            }
        }
        let network = self
            .check_authentication(&response)
            .and_then(|()| self.check_response(&mut response, outcome));
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
            Body::Discovery(_) | Body::Snapshot(_) => {
                let result =
                    network.and_then(|()| Err(anyhow::anyhow!("missing Git HTTP/2 fetch body")));
                finished.push((token, result));
            }
        }
        Ok(())
    }

    fn complete_preparation(
        &mut self,
        token: usize,
        mut response: Easy2<Response>,
        preparation: FetchPreparation,
    ) -> Result<()> {
        match preparation {
            FetchPreparation::NativeHttp1 => {
                self.check_authentication(&response)?;
                return Err(Http1Required.into());
            }
            FetchPreparation::ProbeAuthentication => {
                self.probe_exact_fetch_refs()?;
            }
        }
        response.get_mut().initial_fetch = Some(InitialFetch::AfterRefProbe);
        self.apply_request_headers(&mut response, true)?;
        self.submit(token, response)
    }

    fn probe_exact_fetch_refs(&mut self) -> Result<()> {
        // Startup submits its first OID before admitting other fetches.
        ensure!(self.active.is_empty(), "ref probe overlaps another fetch");
        match self.query_refs(None) {
            Ok(Some(_)) => Ok(()),
            Err(error)
                if error.is::<AuthenticationRejected>() || crate::object::is_cancelled(&error) =>
            {
                Err(error)
            }
            _ => Err(NativeAuthentication.into()),
        }
    }

    fn retry_authentication(&mut self, token: usize, mut response: Easy2<Response>) -> Result<()> {
        if !self.authenticate(&response)? {
            return Err(NativeAuthentication.into());
        }
        self.apply_request_headers(&mut response, true)?;
        self.submit(token, response)
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
        let protocol = if self.allow_http1 { "HTTPS" } else { "HTTP/2" };
        outcome.with_context(|| format!("receive Git {protocol} response"))?;
        let status = response.response_code()?;
        ensure!(status == 200, "Git {protocol} fetch returned HTTP {status}");
        ensure!(
            response.get_ref().supports_protocol()
                && content_type_is(response.content_type_bytes()?, PACK_RESULT),
            "invalid Git {protocol} fetch response"
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

    #[test]
    fn basic_parameters_distinguish_extensions_from_other_schemes() {
        let basic = b"Basic REALM = \"a, \\\"quoted\\\" realm\", charset=UTF-8";
        assert!(replayable_basic_challenge(&[basic.to_vec()]));
        let mixed = b"Basic realm=\"a\", Digest realm=\"b\"";
        assert!(!replayable_basic_challenge(&[mixed.to_vec()]));
    }

    #[test]
    fn a_delayed_unauthenticated_stream_respects_its_own_challenge() {
        let mut client = Client {
            base: String::new(),
            ca: None,
            user_agent: String::new(),
            auth: Some(Credentials {
                response: b"u:p".to_vec(),
                authorization: Authorization::Password {
                    user: 0..1,
                    password: 2..3,
                },
                continuing: false,
            }),
            auth_generation: 1,
            credential: None,
            allow_http1: false,
            initial_fetch: None,
            multi: Multi::new(),
            active: HashMap::new(),
            decoding: HashMap::new(),
            retiring: Vec::new(),
            stop: Arc::default(),
        };
        let mut delayed = Response {
            request: None,
            body: Body::Discovery(Vec::new()),
            error: None,
            stop: Arc::default(),
            authentication: None,
            encoded_continuation: false,
            protocol: Some(HttpProtocol::Http2),
            headers_complete: false,
            initial_fetch: None,
            allow_http1: false,
            ok: false,
            git_pack: false,
            challenges: vec![b"Bearer realm=\"other\"".to_vec()],
        };
        assert!(!client.authenticate_response(401, &delayed).unwrap());
        // A request that did send a continuation token can finish using
        // the newer credential from that exchange.
        delayed.authentication = Some(0);
        delayed.encoded_continuation = true;
        assert!(client.authenticate_response(401, &delayed).unwrap());
        delayed.challenges = vec![b"Basic realm=\"same\"".to_vec()];
        delayed.authentication = None;
        delayed.encoded_continuation = false;
        assert!(client.authenticate_response(401, &delayed).unwrap());
    }

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
                authentication: None,
                encoded_continuation: false,
                protocol: Some(HttpProtocol::Http2),
                headers_complete: false,
                initial_fetch: None,
                allow_http1: false,
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
            allow_http1: false,
            initial_fetch: None,
            auth_generation: 0,
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
            authentication: None,
            encoded_continuation: false,
            protocol: Some(HttpProtocol::Http2),
            headers_complete: false,
            initial_fetch: None,
            allow_http1: false,
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
