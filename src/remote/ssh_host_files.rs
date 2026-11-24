use super::ssh_jump;
use anyhow::{bail, Context, Result};
use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fs,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        process::CommandExt,
    },
    process::{Command, Output, Stdio},
};

const HOST_FILES: &[u8] = b"userknownhostsfile";
const HOST_FILE_DEFAULT: &[u8] = b"~/.ssh/known_hosts ~/.ssh/known_hosts2";
const DISABLE_KEYCHAIN: &str = "UseKeychain=no";

pub(super) struct Launch {
    pub(super) args: Vec<OsString>,
    pub(super) jump_client: OsString,
}

pub(super) fn launch(
    program: &OsStr,
    launcher: Option<&OsStr>,
    args: Vec<OsString>,
    before_host: usize,
) -> Result<Launch> {
    let mut probe = Probe {
        program,
        launcher,
        disable_keychain: cfg!(target_os = "macos"),
    };
    let inspection_args = inspection_args(&args, before_host);
    let settings = probe.initial(&inspection_args)?;
    check_read_only_options(&settings)?;
    let proxy = ssh_jump::from_probe(
        setting(&settings, b"proxyjump"),
        setting(&settings, b"proxycommand"),
        &settings.stderr,
        launcher,
    )?;
    let (check_host_ip, host_files) =
        keep_host_files_read_only(&probe, &args[..before_host], &inspection_args, &settings)?;
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
    let jump_client = if let Some(proxy) = proxy {
        let mut forwarded = Vec::new();
        // OpenSSH disables fd passing for an implicit ProxyJump.
        if proxy.implicit {
            push_option(&mut forwarded, "ProxyUseFdpass=no");
        }
        forwarded.extend(ssh_jump::without_proxy_arguments(&args[..before_host]));
        let user_args = forwarded.len();
        forwarded.extend_from_slice(&args[before_host..]);
        let override_option =
            OsString::from_vec([b"ProxyCommand=", proxy.command.as_slice()].concat());
        let checked = probe.inspect(
            &self::inspection_args(&forwarded, user_args),
            Some(&override_option),
        )?;
        let fdpass = if proxy.implicit {
            Some(b"no".as_slice())
        } else {
            setting(&settings, b"proxyusefdpass")
        };
        anyhow::ensure!(
            setting(&checked, b"proxycommand") == Some(proxy.command.as_slice())
                && setting(&checked, b"proxyusefdpass") == fdpass,
            "SSH wrapper did not forward the owned proxy command",
        );
        anyhow::ensure!(
            settings_without_proxy(&settings).eq(settings_without_proxy(&checked)),
            "SSH wrapper changes its effective configuration with an owned proxy command",
        );
        push_option(&mut options, override_option);
        options.extend(forwarded);
        proxy.client
    } else {
        options.extend(args);
        program.to_owned()
    };
    Ok(Launch {
        args: options,
        jump_client,
    })
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

fn settings_without_proxy(output: &Output) -> impl Iterator<Item = &[u8]> {
    output.stdout.split(|&byte| byte == b'\n').filter(|line| {
        ![
            b"proxyjump ".as_slice(),
            b"proxycommand ",
            b"proxyusefdpass ",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
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
        ssh_jump::configuration_options(args)
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
    let expected = [b"/dev/null ", host_files].concat();
    let mut chosen: Option<(OsString, Vec<Vec<u8>>)> = None;
    for files in candidates {
        let option = [b"UserKnownHostsFile=/dev/null ", files.as_slice()].concat();
        let option = OsStr::from_bytes(&option);
        let query = probe.inspect(inspection_args, Some(option));
        if !query
            .as_ref()
            .is_ok_and(|query| setting(query, HOST_FILES) == Some(expected.as_slice()))
        {
            continue;
        }
        let words = file_words(&files).context("interpret the SSH host-file argument list")?;
        if let Some((_, previous)) = &chosen {
            anyhow::ensure!(
                same_file_list(probe, inspection_args, previous, &words)?,
                "ambiguous SSH UserKnownHostsFile with CheckHostIP=yes"
            );
        } else {
            chosen = Some((option.to_owned(), words));
        }
    }
    chosen
        .map(|(option, _)| (true, Some(option)))
        .context("cannot preserve SSH UserKnownHostsFile lookup with CheckHostIP=yes")
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
    let mut option = b"UserKnownHostsFile=/dev/null \"".to_vec();
    for &byte in file {
        if matches!(byte, b'\\' | b'"') {
            option.push(b'\\');
        }
        option.push(byte);
    }
    option.push(b'"');
    let settings = probe.inspect(args, Some(OsStr::from_bytes(&option)))?;
    setting(&settings, HOST_FILES)
        .and_then(|value| value.strip_prefix(b"/dev/null "))
        .map(<[u8]>::to_vec)
        .context("identify the SSH host-file path")
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
    launcher: Option<&'a OsStr>,
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
        if let Some(launcher) = self.launcher {
            command.arg0(launcher);
        }
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
