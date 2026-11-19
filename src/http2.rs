//! Git protocol v2 exact-object requests multiplexed by an invocation-owned
//! libcurl multi handle. Native Git still selects and verifies the revision.
use crate::{
    object::{check_transfer, OperationCancelled},
    raw_blob::RawBlobs,
    spool::Spool,
    transport,
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
    io::{self, BufReader, Read, Seek, Write},
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

pub(crate) struct Configuration {
    base: String,
    ca: Option<PathBuf>,
    credential: Command,
}

impl Configuration {
    pub(crate) fn new(url: &OsStr, credential: Command) -> Option<Self> {
        let url = url.to_str()?;
        let parsed = url::Url::parse(url).ok()?;
        if parsed.scheme() != "https"
            || parsed.as_str() != url
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || !curl::Version::get().feature_http2()
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

enum Body {
    Discovery(Vec<u8>),
    Reply(Spool),
}

struct Response {
    body: Body,
    error: Option<io::Error>,
    stop: Arc<AtomicBool>,
    h2: bool,
    basic: bool,
}

impl Handler for Response {
    fn write(&mut self, data: &[u8]) -> Result<usize, WriteError> {
        let result = match &mut self.body {
            Body::Discovery(bytes) if bytes.len() + data.len() <= 256 * 1024 => {
                bytes.extend_from_slice(data);
                Ok(())
            }
            Body::Discovery(_) => Err(io::Error::other("Git HTTP discovery response is oversized")),
            Body::Reply(file) => (&*file).write_all(data),
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
            self.basic = false;
            if let Body::Discovery(bytes) = &mut self.body {
                bytes.clear();
            }
        } else if let Some(colon) = data.iter().position(|&byte| byte == b':') {
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
    stop: Arc<AtomicBool>,
}

impl Client {
    pub(crate) fn negotiate(config: Configuration, stop: Arc<AtomicBool>) -> Result<Option<Self>> {
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
            Some(transport::http_post_probe()?),
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
        transport::check_http_post_probe(bytes)?;
        Ok(Some(client))
    }

    fn easy(&self, service: &str, body: Body, request: Option<Vec<u8>>) -> Result<Easy2<Response>> {
        let mut easy = Easy2::new(Response {
            body,
            error: None,
            stop: self.stop.clone(),
            h2: false,
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

    pub(crate) fn start(&mut self, token: usize, oids: &[Oid], scratch: &RawBlobs) -> Result<()> {
        let easy = self.easy(
            "git-upload-pack",
            Body::Reply(scratch.scratch_file()?),
            Some(transport::exact_request(oids)?),
        )?;
        let mut handle = self.multi.add2(easy)?;
        handle.set_token(token)?;
        self.active.insert(token, handle);
        Ok(())
    }

    pub(crate) fn progress(&mut self, scratch: &RawBlobs) -> Result<Vec<(usize, Result<Spool>)>> {
        self.multi.perform()?;
        let mut finished = Vec::new();
        self.multi.messages(|message| {
            if let Some(result) = message.result() {
                finished.push((message.token(), result));
            }
        });
        finished
            .into_iter()
            .map(|(token, outcome)| {
                let token = token?;
                let handle = self
                    .active
                    .remove(&token)
                    .context("unknown Git HTTP/2 request")?;
                let mut response = self.multi.remove2(handle)?;
                let result = self.finish(&mut response, outcome, scratch);
                Ok((token, result))
            })
            .collect()
    }

    fn finish(
        &self,
        response: &mut Easy2<Response>,
        outcome: Result<(), curl::Error>,
        scratch: &RawBlobs,
    ) -> Result<Spool> {
        if self.stop.load(Ordering::Relaxed) {
            return Err(OperationCancelled.into());
        }
        check_transfer()?;
        if let Some(error) = response.get_mut().error.take() {
            return Err(error.into());
        }
        outcome.context("receive Git HTTP/2 response")?;
        let status = response.response_code()?;
        ensure!(status == 200, "Git HTTP/2 fetch returned HTTP {status}");
        ensure!(
            response.get_ref().h2 && response.content_type_bytes()? == Some(PACK_RESULT),
            "invalid Git HTTP/2 fetch response"
        );
        let Body::Reply(reply) = &response.get_ref().body else {
            bail!("missing Git HTTP/2 fetch body");
        };
        let mut reply = reply;
        reply.rewind()?;
        let pack = scratch.scratch_file()?;
        transport::read_http_pack(BufReader::new(reply), &pack)?;
        Ok(pack)
    }

    pub(crate) fn wait(&self, wake: &UnixStream, timeout: Duration) -> Result<()> {
        let mut fd = WaitFd::new();
        fd.set_fd(wake.as_raw_fd());
        fd.poll_on_read(true);
        self.multi.wait(&mut [fd], timeout)?;
        Ok(())
    }
}
