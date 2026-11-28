//! Git protocol v2 exact-object requests multiplexed by an invocation-owned
//! libcurl multi handle. Native Git still selects and verifies the revision.
use crate::{
    demand::ResponseIdle,
    object::{check_transfer, OperationCancelled},
    pack_stream, transport,
};
use anyhow::{bail, ensure, Context, Result};
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
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

const ADVERTISEMENT: &[u8] = b"application/x-git-upload-pack-advertisement";
const PACK_RESULT: &[u8] = b"application/x-git-upload-pack-result";
const DISCOVERY: &str = "info/refs?service=git-upload-pack";

mod decoder;
use decoder::Decoder;

pub(crate) struct Configuration {
    base: url::Url,
    options: crate::git::HttpsOptions,
    credential: (Command, url::Url),
}

impl Configuration {
    pub(crate) fn new(url: &OsStr, git: &crate::git::Git) -> Result<Self> {
        let mut parsed = url::Url::parse(url.to_str().context("invalid HTTPS URL")?)
            .context("invalid HTTPS URL")?;
        let version = curl::Version::get();
        ensure!(
            parsed.scheme() == "https" && parsed.query().is_none() && parsed.fragment().is_none(),
            "unsupported Git HTTPS URL"
        );
        ensure!(
            version.feature_ssl() && version.feature_http2(),
            "direct HTTPS requires TLS and HTTP/2 support"
        );
        let credential = (git.command(None), parsed.clone());
        parsed
            .set_password(None)
            .map_err(|_| anyhow::anyhow!("invalid HTTPS authority"))?;
        let options = git.https_options(OsStr::new(parsed.as_str()))?;
        parsed
            .set_username("")
            .map_err(|_| anyhow::anyhow!("invalid HTTPS authority"))?;
        Ok(Self {
            base: parsed,
            options,
            credential,
        })
    }
}

struct Credentials {
    user: String,
    password: String,
}

fn credentials(mut command: Command, url: &str) -> Result<Option<Credentials>> {
    crate::git::read_only(&mut command);
    if !crate::terminal_ui::preparing() {
        command
            .args(["-c", "credential.interactive=false"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "/bin/false");
    } else if !crate::terminal_ui::owns_terminal() {
        command.env("GIT_TERMINAL_PROMPT", "0");
    }
    command.args(["credential", "fill"]);
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

enum Body {
    Discovery(Vec<u8>),
    Reply(Box<pack_stream::Response>),
    Streaming(Decoder),
}

struct Response {
    body: Body,
    error: Option<io::Error>,
    stop: Arc<AtomicBool>,
    idle: Option<ResponseIdle>,
    h2: bool,
    ok: bool,
    git_pack: bool,
    basic: bool,
    sent_credentials: bool,
}

impl Handler for Response {
    fn write(&mut self, data: &[u8]) -> Result<usize, WriteError> {
        let result = match &mut self.body {
            Body::Discovery(bytes) if bytes.len() + data.len() <= 256 * 1024 => {
                bytes.extend_from_slice(data);
                Ok(())
            }
            Body::Discovery(_) => Err(io::Error::other("Git HTTP discovery response is oversized")),
            Body::Reply(stream) if self.h2 && self.ok && self.git_pack => {
                stream.write_http(data).map_err(io::Error::other)
            }
            Body::Streaming(decoder) if self.h2 && self.ok && self.git_pack => {
                return decoder.push(data);
            }
            Body::Reply(_) | Body::Streaming(_) => Ok(()),
        };
        match result {
            Ok(()) => Ok(data.len()),
            Err(error) => {
                self.error = Some(error);
                Ok(0)
            }
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
    base: url::Url,
    ca: Option<PathBuf>,
    extra_header: Option<String>,
    user_agent: Option<String>,
    auth: Option<Credentials>,
    credential: Option<(Command, url::Url)>,
    multi: Multi,
    active: HashMap<usize, Easy2Handle<Response>>,
    decoding: HashMap<usize, (Decoder, Result<()>)>,
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
            ca: config.options.ca,
            extra_header: config.options.extra_header,
            user_agent: config.options.user_agent,
            auth: None,
            credential: Some(config.credential),
            multi,
            active: HashMap::new(),
            decoding: HashMap::new(),
            stop,
        };
        let mut response = client.discovery(DISCOVERY, None)?;
        if config.options.follow_initial_redirects {
            for _ in 0..5 {
                let Some(next) = response.redirect_url()? else {
                    break;
                };
                let next = url::Url::parse(next).context("invalid Git HTTPS redirect")?;
                ensure!(
                    next.origin() == client.base.origin()
                        && next.username().is_empty()
                        && next.password().is_none(),
                    "Git HTTPS discovery redirect changed authority"
                );
                let base = next
                    .as_str()
                    .strip_suffix(DISCOVERY)
                    .filter(|base| base.ends_with('/'))
                    .context("invalid Git HTTPS discovery redirect")?;
                client.base = url::Url::parse(base)?;
                if let Some((_, credential_url)) = &mut client.credential {
                    credential_url.set_path(client.base.path().trim_end_matches('/'));
                }
                response = client.discovery(DISCOVERY, None)?;
            }
        }
        ensure!(
            response.response_code()? != 401,
            "Git HTTPS authentication failed"
        );
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
        // A fixed OID needs no ref listing. If the server challenges a POST,
        // retry that request with the credentials obtained through Git.
        let Some(branch) = branch else {
            return Ok(Some((client, None)));
        };
        let response = client.discovery(
            "git-upload-pack",
            Some(transport::reference_request(branch)?),
        )?;
        ensure!(
            response.response_code()? != 401,
            "Git HTTPS authentication failed"
        );
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
        Ok(selected.map(|oid| (client, Some(oid))))
    }

    fn easy(&self, service: &str, body: Body, request: Option<Vec<u8>>) -> Result<Easy2<Response>> {
        let mut easy = Easy2::new(Response {
            body,
            error: None,
            stop: self.stop.clone(),
            idle: None,
            h2: false,
            ok: false,
            git_pack: false,
            basic: false,
            sent_credentials: false,
        });
        easy.url(&format!(
            "{}/{service}",
            self.base.as_str().trim_end_matches('/')
        ))?;
        easy.proxy("")?;
        easy.http_version(HttpVersion::V2TLS)?;
        easy.pipewait(true)?;
        easy.progress(true)?;
        if let Some(ca) = &self.ca {
            easy.cainfo(ca)?;
        }
        if let Some(agent) = &self.user_agent {
            easy.useragent(agent)?;
        }
        if let Some(auth) = &self.auth {
            Self::set_credentials(&mut easy, auth)?;
        }
        let mut headers = List::new();
        headers.append("Git-Protocol: version=2")?;
        if let Some(header) = &self.extra_header {
            headers.append(header)?;
        }
        if let Some(request) = request {
            headers.append("Content-Type: application/x-git-upload-pack-request")?;
            headers.append("Accept: application/x-git-upload-pack-result")?;
            easy.post_fields_copy(&request)?;
        }
        easy.http_headers(headers)?;
        Ok(easy)
    }

    fn set_credentials(easy: &mut Easy2<Response>, auth: &Credentials) -> Result<()> {
        easy.username(&auth.user)?;
        easy.password(&auth.password)?;
        easy.http_auth(Auth::new().basic(true))?;
        easy.get_mut().sent_credentials = true;
        Ok(())
    }

    fn retry_with_credentials(&mut self, response: &mut Easy2<Response>) -> Result<bool> {
        let state = response.get_ref();
        if response.response_code()? != 401 || !state.h2 || !state.basic || state.sent_credentials {
            return Ok(false);
        }
        if let Some((command, url)) = self.credential.take() {
            self.auth = credentials(command, url.as_str())?;
        }
        let Some(auth) = &self.auth else {
            return Ok(false);
        };
        Self::set_credentials(response, auth)?;
        Ok(true)
    }

    fn discovery(&mut self, service: &str, request: Option<Vec<u8>>) -> Result<Easy2<Response>> {
        let easy = self.easy(service, Body::Discovery(Vec::new()), request)?;
        let mut response = self.metadata(easy)?;
        if self.retry_with_credentials(&mut response)? {
            return self.metadata(response);
        }
        Ok(response)
    }

    fn metadata(&self, easy: Easy2<Response>) -> Result<Easy2<Response>> {
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
        idle: ResponseIdle,
    ) -> Result<()> {
        let decoder = Decoder::new(response, self.stop.clone(), wake.clone())?;
        let body = Body::Streaming(decoder);
        let mut easy = self.easy(
            "git-upload-pack",
            body,
            Some(transport::exact_request(oids)?),
        )?;
        easy.get_mut().idle = Some(idle);
        self.enqueue(token, easy)
    }

    pub(crate) fn fetch_eager(
        &mut self,
        oid: Oid,
        response: pack_stream::Response,
    ) -> Result<pack_stream::Received> {
        self.start_request(
            0,
            transport::fetch_request(&[oid], true)?,
            Body::Reply(Box::new(response)),
        )?;
        loop {
            check_transfer()?;
            if let Some((_, result)) = self.progress(false)?.into_iter().next() {
                return result;
            }
            self.multi.wait(&mut [], Duration::from_millis(100))?;
        }
    }

    fn start_request(&mut self, token: usize, request: Vec<u8>, response: Body) -> Result<()> {
        let easy = self.easy("git-upload-pack", response, Some(request))?;
        self.enqueue(token, easy)
    }

    fn enqueue(&mut self, token: usize, easy: Easy2<Response>) -> Result<()> {
        let mut handle = self.multi.add2(easy)?;
        handle.set_token(token)?;
        self.active.insert(token, handle);
        Ok(())
    }

    pub(crate) fn progress(
        &mut self,
        finishing: bool,
    ) -> Result<Vec<(usize, Result<pack_stream::Received>)>> {
        let mut results = self.poll_active_decoders()?;
        self.multi.perform()?;
        let mut finished = Vec::new();
        self.multi.messages(|message| {
            if let Some(result) = message.result() {
                finished.push((message.token(), result));
            }
        });
        for (token, outcome) in finished {
            self.finish_request(token?, outcome, &mut results)?;
        }
        if finishing {
            self.expire_receives(&mut results)?;
        }
        let complete: Vec<_> = self
            .decoding
            .iter()
            .filter_map(|(&token, (decoder, _))| decoder.result().map(|result| (token, result)))
            .collect();
        for (token, result) in complete {
            let (_, transfer) = self.decoding.remove(&token).unwrap();
            let result = match result {
                Err(error) if !crate::object::is_cancelled(&error) => Err(error),
                result => transfer.and(result),
            };
            results.push((token, result));
        }
        Ok(results)
    }

    fn expire_receives(
        &mut self,
        results: &mut Vec<(usize, Result<pack_stream::Received>)>,
    ) -> Result<()> {
        // Drain curl's input and completion messages before testing for idle
        // streams. A paused write callback is waiting for local processing.
        let mut expired = Vec::new();
        for (&token, handle) in &mut self.active {
            let response = handle.get_mut();
            let (Body::Streaming(decoder), Some(idle)) = (&response.body, &mut response.idle)
            else {
                continue;
            };
            if decoder.is_paused() {
                idle.reset();
            } else if decoder.has_pending_input() {
                continue;
            } else if let Err(error) = idle.check() {
                response.error = Some(error);
                expired.push(token);
            }
        }
        for token in expired {
            self.finish_request(token, Ok(()), results)?;
        }
        Ok(())
    }

    fn poll_active_decoders(&mut self) -> Result<Vec<(usize, Result<pack_stream::Received>)>> {
        let mut stopped = Vec::new();
        let mut resume_errors = Vec::new();
        for (&token, handle) in &mut self.active {
            let Body::Streaming(decoder) = &mut handle.get_mut().body else {
                continue;
            };
            // The worker can fail while curl is paused with a full queue.
            // A successful decoder cannot finish before its input is closed.
            if let Some(result) = decoder.result() {
                stopped.push((
                    token,
                    result.and_then(|_| bail!("Git pack decoder finished before its response")),
                ));
            } else if decoder.resume() {
                if let Err(error) = handle.unpause_write() {
                    resume_errors.push((token, error));
                }
            }
        }
        for (token, _) in &stopped {
            self.multi.remove2(self.active.remove(token).unwrap())?;
        }
        for (token, error) in resume_errors {
            self.finish_request(token, Err(error), &mut stopped)?;
        }
        Ok(stopped)
    }

    fn finish_request(
        &mut self,
        token: usize,
        outcome: Result<(), curl::Error>,
        results: &mut Vec<(usize, Result<pack_stream::Received>)>,
    ) -> Result<()> {
        let handle = self
            .active
            .remove(&token)
            .context("unknown Git HTTP/2 request")?;
        let mut response = self.multi.remove2(handle)?;
        if outcome.is_ok()
            && response.get_ref().error.is_none()
            && self.retry_with_credentials(&mut response)?
        {
            return self.enqueue(token, response);
        }
        let outcome = self.check_response(&mut response, outcome);
        let body = std::mem::replace(&mut response.get_mut().body, Body::Discovery(Vec::new()));
        match body {
            Body::Streaming(mut decoder) => {
                decoder.close(outcome.is_ok());
                self.decoding.insert(token, (decoder, outcome));
            }
            Body::Reply(stream) => {
                results.push((token, outcome.and_then(|()| stream.finish_http())))
            }
            Body::Discovery(_) => bail!("missing Git HTTP/2 fetch body"),
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
