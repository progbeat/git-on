use anyhow::{bail, Context, Result};
use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fs, io,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::Path,
    process::{Command, Output, Stdio},
};

const HOST_FILES: &[u8] = b"userknownhostsfile";
const HOST_FILE_DEFAULT: &[u8] = b"~/.ssh/known_hosts ~/.ssh/known_hosts2";
const MAX_HOST_FILES: usize = 32;
const DISABLE_KEYCHAIN: &str = "UseKeychain=no";

pub(super) fn launch(
    program: &OsStr,
    launcher_path: &Path,
    args: Vec<OsString>,
    before_host: usize,
) -> Result<Vec<OsString>> {
    let mut probe = Probe {
        program,
        disable_keychain: cfg!(target_os = "macos"),
    };
    let inspection_args = inspection_args(&args, before_host);
    let settings = probe.initial(&inspection_args)?;
    check_read_only_options(&settings)?;
    for key in [b"proxyjump".as_slice(), b"proxycommand"] {
        anyhow::ensure!(
            setting(&settings, key).is_none_or(|value| value == b"none"),
            "direct SSH does not support ProxyJump or ProxyCommand"
        );
    }
    let (check_host_ip, host_files) = keep_host_files_read_only(
        &probe,
        &args[..before_host],
        &inspection_args,
        &settings,
        launcher_path
            .parent()
            .context("missing private SSH directory")?,
    )?;
    let mut options = Vec::new();
    if probe.disable_keychain {
        push_option(&mut options, DISABLE_KEYCHAIN);
    }
    push_option(
        &mut options,
        if check_host_ip {
            "CheckHostIP=yes"
        } else {
            "CheckHostIP=no"
        },
    );
    if let Some(host_files) = host_files {
        push_option(&mut options, host_files);
    }
    // Pin direct transport in the actual invocation, after explicit caller
    // options (including -J none) and before OpenSSH reads config again.
    let mut args = args.into_iter();
    options.extend(args.by_ref().take(before_host));
    push_option(&mut options, "ProxyCommand=none");
    options.extend(args);
    Ok(options)
}

fn check_read_only_options(settings: &Output) -> Result<()> {
    for option in super::SSH_READ_ONLY.split_ascii_whitespace() {
        let (key, expected) = option.split_once('=').expect("read-only SSH setting");
        let effective = setting(settings, key.to_ascii_lowercase().as_bytes());
        let effective = match effective {
            Some(b"yes" | b"true") => Some("yes"),
            Some(b"no" | b"false") => Some("no"),
            _ => None,
        };
        anyhow::ensure!(
            effective == Some(expected),
            "SSH wrapper did not preserve the read-only setting {option}"
        );
    }
    Ok(())
}

// -o can follow another flag in the same option word.
fn configuration_options(args: &[OsString]) -> impl Iterator<Item = &[u8]> {
    let mut args = args.iter();
    std::iter::from_fn(move || loop {
        let argument = args.next()?;
        let Some(flags) = argument.as_bytes().strip_prefix(b"-") else {
            continue;
        };
        let Some(index) = flags
            .iter()
            .position(|flag| b"BbcDEeFIiJLlmOoPpQRSWw".contains(flag))
        else {
            continue;
        };
        let (flag, inline) = flags[index..].split_first().unwrap();
        let value = if inline.is_empty() {
            args.next()?.as_bytes()
        } else {
            inline
        };
        if *flag == b'o' {
            return Some(value);
        }
    })
}

fn push_option(args: &mut Vec<OsString>, value: impl Into<OsString>) {
    args.extend([OsString::from("-o"), value.into()]);
}

fn inspection_args(args: &[OsString], before_host: usize) -> Vec<OsString> {
    // -q can occur in a short-option group. Set probe verbosity after
    // those options, preserving the quiet and verbosity flags of the transport.
    let mut probe_args = args.to_vec();
    probe_args.insert(before_host, "-vvvvvv".into());
    probe_args
}

fn keep_host_files_read_only(
    probe: &Probe<'_>,
    args: &[OsString],
    inspection_args: &[OsString],
    settings: &Output,
    scratch: &Path,
) -> Result<(bool, Option<OsString>)> {
    match setting(settings, b"checkhostip") {
        Some(b"no") => return Ok((false, None)),
        Some(b"yes") => (),
        _ => bail!("OpenSSH did not report its effective CheckHostIP setting"),
    }
    let Some(host_files) = setting(settings, HOST_FILES).filter(|files| *files != b"none") else {
        return Ok((true, Some("UserKnownHostsFile=none".into())));
    };

    // OpenSSH only appends to the first user file. Its -G output flattens
    // the paths without quotes, so use the original option text. Never
    // interpret whitespace in the dump as a path separator.
    let mut candidates = BTreeSet::from([HOST_FILE_DEFAULT.to_vec()]);
    candidates.extend(
        configuration_options(args)
            .filter_map(host_file_values)
            .map(<[u8]>::to_vec),
    );
    for path in settings
        .stderr
        .split(|&byte| byte == b'\n')
        .filter_map(|line| line.strip_prefix(b"debug1: Reading configuration data "))
    {
        let path = OsStr::from_bytes(path.strip_suffix(b"\r").unwrap_or(path));
        let config = fs::read(path).with_context(|| format!("read SSH configuration {path:?}"))?;
        candidates.extend(
            config
                .split(|&byte| byte == b'\n')
                .filter_map(host_file_values)
                .map(<[u8]>::to_vec),
        );
    }
    let with_sink = [b"/dev/null ", host_files].concat();
    let mut chosen: Option<(Vec<u8>, Vec<Vec<u8>>)> = None;
    for files in candidates {
        // Plain paths need no expansion or quote interpretation. Exclude
        // unrelated host stanzas before asking ssh to re-read the config.
        if files
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/.-_: \t".contains(byte))
            && !files
                .split(|byte| byte.is_ascii_whitespace())
                .filter(|word| !word.is_empty())
                .eq(host_files.split(|&byte| byte == b' '))
        {
            continue;
        }
        let Some(words) = file_words(&files) else {
            continue;
        };
        // A literal final component is independent of expansions in its directory.
        let tail = words.last().and_then(|word| {
            word.iter()
                .rposition(|&byte| byte == b'/')
                .map(|slash| &word[slash..])
        });
        if tail.is_some_and(|tail| {
            !tail.iter().any(|byte| b"%${}~".contains(byte)) && !host_files.ends_with(tail)
        }) {
            continue;
        }
        let (prefix, expected) = if words.len() < MAX_HOST_FILES {
            (
                b"UserKnownHostsFile=/dev/null ".as_slice(),
                with_sink.as_slice(),
            )
        } else {
            (b"UserKnownHostsFile=".as_slice(), host_files)
        };
        let option = [prefix, files.as_slice()].concat();
        let query = probe.inspect(inspection_args, Some(OsStr::from_bytes(&option)));
        if !query
            .as_ref()
            .is_ok_and(|query| setting(query, HOST_FILES) == Some(expected))
        {
            continue;
        }
        if let Some((_, previous)) = &chosen {
            anyhow::ensure!(
                same_file_list(probe, inspection_args, previous, &words)?,
                "ambiguous SSH UserKnownHostsFile with CheckHostIP=yes"
            );
        } else {
            chosen = Some((option, words));
        }
    }
    let (mut option, words) =
        chosen.context("cannot preserve SSH UserKnownHostsFile lookup with CheckHostIP=yes")?;
    if words.len() >= MAX_HOST_FILES {
        option = replace_first_host_file(probe, inspection_args, scratch, host_files, &words)?;
    }
    Ok((true, Some(OsString::from_vec(option))))
}

fn replace_first_host_file(
    probe: &Probe<'_>,
    args: &[OsString],
    scratch: &Path,
    effective: &[u8],
    words: &[Vec<u8>],
) -> Result<Vec<u8>> {
    // OpenSSH accepts 32 user files. Preserve the first file's entries
    // at that limit by placing only its append target in service storage.
    let (first, rest) = words.split_first().context("missing first SSH host file")?;
    let source = inspect_file(probe, args, first)?;
    let remaining = effective
        .strip_prefix(source.as_slice())
        .and_then(|value| value.strip_prefix(b" "))
        .context("could not identify first SSH host file")?;
    let mut private = tempfile::NamedTempFile::new_in(scratch)?;
    match fs::File::open(OsStr::from_bytes(&source)) {
        Ok(mut input) => {
            io::copy(&mut input, private.as_file_mut())?;
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) => {}
        Err(error) => return Err(error.into()),
    }
    let path = private.path().as_os_str().as_bytes();
    let mut option = b"UserKnownHostsFile=".to_vec();
    let mut escaped = Vec::with_capacity(path.len());
    for &byte in path {
        if byte == b'%' {
            escaped.push(byte);
        }
        escaped.push(byte);
    }
    quote_host_file(&mut option, &escaped);
    for file in rest {
        option.push(b' ');
        quote_host_file(&mut option, file);
    }
    let expected = [path, b" ", remaining].concat();
    let checked = probe.inspect(args, Some(OsStr::from_bytes(&option)))?;
    anyhow::ensure!(
        setting(&checked, HOST_FILES) == Some(&expected),
        "could not preserve SSH host file order"
    );
    private.keep()?;
    Ok(option)
}

fn same_file_list(
    probe: &Probe<'_>,
    args: &[OsString],
    a: &[Vec<u8>],
    b: &[Vec<u8>],
) -> Result<bool> {
    if a.len() != b.len() {
        return Ok(false);
    }
    // One path has no boundary to disambiguate in the matching native output.
    if a.len() == 1 {
        return Ok(true);
    }
    for (a, b) in a.iter().zip(b).filter(|(a, b)| a != b) {
        let a = inspect_file(probe, args, a)?;
        let b = inspect_file(probe, args, b)?;
        if a != b {
            return Ok(false);
        }
    }
    Ok(true)
}

fn inspect_file(probe: &Probe<'_>, args: &[OsString], file: &[u8]) -> Result<Vec<u8>> {
    let mut option = b"UserKnownHostsFile=/dev/null ".to_vec();
    quote_host_file(&mut option, file);
    let settings = probe.inspect(args, Some(OsStr::from_bytes(&option)))?;
    setting(&settings, HOST_FILES)
        .and_then(|value| value.strip_prefix(b"/dev/null "))
        .map(<[u8]>::to_vec)
        .context("identify the SSH host-file path")
}

fn quote_host_file(option: &mut Vec<u8>, file: &[u8]) {
    option.push(b'"');
    for &byte in file {
        if matches!(byte, b'\\' | b'"') {
            option.push(b'\\');
        }
        option.push(byte);
    }
    option.push(b'"');
}

// Extract boundaries from the original OpenSSH argument list. Only -G
// supplies the effective values. This comparison never selects new files.
pub(super) fn file_words(values: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut words = Vec::new();
    let mut bytes = values.iter().copied().peekable();
    loop {
        while bytes
            .next_if(|&byte| matches!(byte, b' ' | b'\t'))
            .is_some()
        {}
        if matches!(bytes.peek(), None | Some(b'#')) {
            return Some(words);
        }
        let mut word = Vec::new();
        let mut quote = None;
        while let Some(byte) = bytes.next() {
            match byte {
                b' ' | b'\t' if quote.is_none() => break,
                b'\\' => {
                    word.push(
                        bytes
                            .next_if(|&next| {
                                matches!(next, b'\\' | b'\'' | b'"')
                                    || next == b' ' && quote.is_none()
                            })
                            .unwrap_or(byte),
                    );
                }
                b'\'' | b'"' if quote.is_none() => quote = Some(byte),
                byte if quote == Some(byte) => quote = None,
                byte => word.push(byte),
            }
        }
        if quote.is_some() {
            return None;
        }
        words.push(word);
    }
}

struct Probe<'a> {
    program: &'a OsStr,
    disable_keychain: bool,
}

impl Probe<'_> {
    fn initial(&mut self, args: &[OsString]) -> Result<Output> {
        let output = self.capture(args, None)?;
        // Portable OpenSSH on macOS lacks the Apple keychain option.
        // Let the caller's configuration supply its own IgnoreUnknown.
        if self.disable_keychain
            && !output.status.success()
            && output.stderr.split(|&byte| byte == b'\n').any(|line| {
                line.trim_ascii_end()
                    .ends_with(b"Bad configuration option: usekeychain")
            })
        {
            self.disable_keychain = false;
            return self.inspect(args, None);
        }
        Self::checked(output)
    }

    fn inspect(&self, args: &[OsString], option: Option<&OsStr>) -> Result<Output> {
        Self::checked(self.capture(args, option)?)
    }

    fn capture(&self, args: &[OsString], option: Option<&OsStr>) -> Result<Output> {
        let mut command = Command::new(self.program);
        command.arg("-G");
        if self.disable_keychain {
            command.args(["-o", DISABLE_KEYCHAIN]);
        }
        if let Some(option) = option {
            command.arg("-o").arg(option);
        }
        command.args(args).stdin(Stdio::null());
        crate::git::capture_inherited_group(command)
    }

    fn checked(output: Output) -> Result<Output> {
        if !output.status.success() {
            bail!(
                "read effective SSH configuration: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output)
    }
}

fn setting<'a>(settings: &'a Output, key: &[u8]) -> Option<&'a [u8]> {
    settings
        .stdout
        .split(|&byte| byte == b'\n')
        .find_map(|line| {
            let value = line.strip_prefix(key)?.strip_prefix(b" ")?;
            Some(value.strip_suffix(b"\r").unwrap_or(value))
        })
}

fn host_file_values(line: &[u8]) -> Option<&[u8]> {
    let line = line.trim_ascii_start();
    let split = line
        .iter()
        .position(|&byte| byte == b'=' || byte.is_ascii_whitespace())?;
    if !line[..split].eq_ignore_ascii_case(HOST_FILES) {
        return None;
    }
    let values = line[split..].trim_ascii_start();
    Some(values.strip_prefix(b"=").unwrap_or(values).trim_ascii())
}
