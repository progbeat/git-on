use super::ssh_host_files::file_words;
use anyhow::{bail, Context, Result};
use std::{
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
};

pub(super) struct Proxy {
    pub(super) client: OsString,
    pub(super) command: Vec<u8>,
    pub(super) implicit: bool,
}

pub(super) fn from_probe(
    jump: Option<&[u8]>,
    proxy: Option<&[u8]>,
    diagnostics: &[u8],
    launcher: Option<&OsStr>,
) -> Result<Option<Proxy>> {
    if jump.is_none_or(|jump| jump.is_empty() || jump == b"none") {
        return explicit_proxy(proxy, launcher);
    }
    let implicit = diagnostics.rsplit(|&byte| byte == b'\n').find_map(|line| {
        line.strip_prefix(b"debug1: Setting implicit ProxyCommand from ProxyJump: ")
    });
    let launcher = launcher
        .context("choose a GIT_ON_SERVICE_TMPDIR without shell characters for ProxyJump")?;
    let implicit = implicit.context("OpenSSH did not report its implicit jump command")?;
    let implicit = implicit.strip_suffix(b"\r").unwrap_or(implicit);
    let words = file_words(implicit).context("read OpenSSH jump arguments")?;
    let (client, args) = words
        .split_first()
        .context("OpenSSH reported an empty jump command")?;
    if client == launcher.as_bytes() {
        return Ok(None);
    }

    // A forwarding wrapper changed argv[0]. Keep native jump arguments but
    // send the connection through the owned launcher. Rebuild argv from the
    // known OpenSSH layout rather than treating diagnostic text as a command.
    let mut command = Vec::new();
    add_argument(&mut command, launcher.as_bytes());
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        match flag.as_slice() {
            b"-l" | b"-p" | b"-F" | b"-J" => {
                let value = args.next().context("OpenSSH jump option needs a value")?;
                add_argument(&mut command, flag);
                add_argument(&mut command, value);
            }
            b"-v" | b"-vv" | b"-vvv" => (),
            b"-W" => {
                let address = args
                    .next()
                    .context("OpenSSH jump needs a forwarding address")?;
                anyhow::ensure!(
                    address == b"[%h]:%p",
                    "unexpected OpenSSH jump forwarding address"
                );
                let host = args.next().context("OpenSSH jump needs a host")?;
                anyhow::ensure!(
                    args.next().is_none(),
                    "unexpected trailing OpenSSH jump arguments"
                );
                command.extend_from_slice(b" -W '[%h]:%p' --");
                add_argument(&mut command, host);
                return Ok(Some(Proxy {
                    client: OsString::from_vec(client.clone()),
                    command,
                    implicit: true,
                }));
            }
            _ => bail!("unexpected option in OpenSSH's implicit jump command"),
        }
    }
    bail!("OpenSSH reported an incomplete jump command")
}

fn explicit_proxy(proxy: Option<&[u8]>, launcher: Option<&OsStr>) -> Result<Option<Proxy>> {
    let Some(proxy) = proxy.filter(|value| !value.is_empty() && *value != b"none") else {
        return Ok(None);
    };
    super::validate_ssh_command(OsStr::from_bytes(proxy)).context("read SSH ProxyCommand")?;
    anyhow::ensure!(
        !proxy
            .iter()
            .any(|&byte| matches!(byte, b'$' | b'~' | b'*' | b'?')),
        "SSH ProxyCommand requires literal arguments"
    );
    let words = file_words(proxy).context("read SSH ProxyCommand arguments")?;
    let words = words.strip_prefix(&[b"exec".to_vec()]).unwrap_or(&words);
    let (client, args) = words.split_first().context("empty SSH ProxyCommand")?;
    let program = std::path::Path::new(OsStr::from_bytes(client)).file_name();
    if program.is_some_and(|name| name == "nc" || name == "nc.openbsd") {
        validate_nc(args)?;
        return Ok(None);
    }
    anyhow::ensure!(
        program == Some(OsStr::new("ssh")),
        "SSH ProxyCommand must use ssh -W or a literal nc relay"
    );
    let (host, args) = args
        .split_last()
        .context("SSH ProxyCommand needs a forwarding host")?;
    anyhow::ensure!(
        !host.is_empty() && !host.starts_with(b"-") && !host.contains(&b'%'),
        "SSH ProxyCommand requires a literal forwarding host"
    );
    let launcher = launcher
        .context("choose a GIT_ON_SERVICE_TMPDIR without shell characters for ProxyCommand")?;
    let mut command = Vec::new();
    add_argument(&mut command, launcher.as_bytes());
    let mut forwards = false;
    let args: Vec<_> = args.iter().cloned().map(OsString::from_vec).collect();
    let args = args.strip_suffix(&[OsString::from("--")]).unwrap_or(&args);
    for (argument, option) in short_arguments(args) {
        let flags = argument
            .as_bytes()
            .strip_prefix(b"-")
            .context("invalid SSH ProxyCommand argument before the forwarding host")?;
        let short = option
            .as_ref()
            .map_or(flags, |option| &flags[..option.index]);
        anyhow::ensure!(
            short.iter().all(|byte| b"46qv".contains(byte)),
            "unsupported SSH ProxyCommand option"
        );
        if let Some(option) = option {
            let value = option.value();
            anyhow::ensure!(!value.is_empty(), "SSH ProxyCommand option needs a value");
            if option.flag == b'W' {
                anyhow::ensure!(
                    !forwards,
                    "SSH ProxyCommand has multiple forwarding addresses"
                );
                forwards = true;
                if matches!(value, b"%h:%p" | b"[%h]:%p") {
                    if !short.is_empty() {
                        add_argument(&mut command, &[b"-", short].concat());
                    }
                    command.extend_from_slice(b" -W '[%h]:%p'");
                    continue;
                }
            } else {
                anyhow::ensure!(
                    b"FiJlop".contains(&option.flag),
                    "unsupported SSH ProxyCommand option"
                );
            }
            anyhow::ensure!(
                !value.contains(&b'%'),
                "unsupported token in SSH ProxyCommand options"
            );
            add_argument(&mut command, argument.as_bytes());
            if let Some(value) = option.separate {
                add_argument(&mut command, value.as_bytes());
            }
        } else {
            anyhow::ensure!(!flags.is_empty(), "empty SSH ProxyCommand option");
            add_argument(&mut command, argument.as_bytes());
        }
    }
    anyhow::ensure!(
        forwards,
        "SSH ProxyCommand must specify a forwarding address (-W)"
    );
    command.extend_from_slice(b" --");
    add_argument(&mut command, host);
    Ok(Some(Proxy {
        client: OsString::from_vec(client.clone()),
        command,
        implicit: false,
    }))
}

fn validate_nc(args: &[Vec<u8>]) -> Result<()> {
    let [options @ .., host, port] = args else {
        bail!("nc ProxyCommand requires the destination host and port");
    };
    let literal = |word: &[u8]| {
        !word.is_empty()
            && !word.starts_with(b"-")
            && !word
                .iter()
                .any(|&byte| byte.is_ascii_whitespace() || byte == b'%')
    };
    let numeric = |word: &[u8]| !word.is_empty() && word.iter().all(u8::is_ascii_digit);
    anyhow::ensure!(
        (host == b"%h" || literal(host)) && (port == b"%p" || numeric(port)),
        "unsupported nc ProxyCommand destination"
    );
    let mut options = options.iter();
    while let Some(option) = options.next() {
        let valid = match option.as_slice() {
            b"-x" => options.next().is_some_and(|value| literal(value)),
            b"-X" => options
                .next()
                .is_some_and(|value| matches!(value.as_slice(), b"4" | b"5" | b"connect")),
            b"-w" => options.next().is_some_and(|value| numeric(value)),
            _ => option.strip_prefix(b"-").is_some_and(|flags| {
                !flags.is_empty() && flags.iter().all(|flag| b"46Fnv".contains(flag))
            }),
        };
        anyhow::ensure!(valid, "unsupported nc ProxyCommand option");
    }
    Ok(())
}

fn add_argument(command: &mut Vec<u8>, argument: &[u8]) {
    if !command.is_empty() {
        command.push(b' ');
    }
    let mut literal = Vec::with_capacity(argument.len());
    for &byte in argument {
        if byte == b'%' {
            literal.push(byte);
        }
        literal.push(byte);
    }
    command.extend_from_slice(&gix_quote::single(literal.as_slice().into()));
}

pub(super) fn without_proxy_arguments(args: &[OsString]) -> Vec<OsString> {
    let mut filtered = Vec::with_capacity(args.len());
    for (argument, option) in short_arguments(args) {
        if let Some(option) = option {
            let keyword = option
                .value()
                .trim_ascii_start()
                .split(|&byte| byte.is_ascii_whitespace() || byte == b'=')
                .next();
            if option.flag == b'J'
                || option.flag == b'o'
                    && keyword.is_some_and(|word| {
                        word.eq_ignore_ascii_case(b"ProxyJump")
                            || word.eq_ignore_ascii_case(b"ProxyCommand")
                    })
            {
                if option.index != 0 {
                    let flags = &argument.as_bytes()[..option.index + 1];
                    filtered.push(OsString::from_vec(flags.to_vec()));
                }
                continue;
            }
            filtered.extend(std::iter::once(argument.clone()).chain(option.separate.cloned()));
        } else {
            filtered.push(argument.clone());
        }
    }
    filtered
}

pub(super) fn configuration_options(args: &[OsString]) -> impl Iterator<Item = &[u8]> {
    short_arguments(args).filter_map(|(_, option)| {
        let option = option.filter(|option| option.flag == b'o')?;
        Some(option.value())
    })
}

struct ValueOption<'a> {
    index: usize,
    flag: u8,
    inline: &'a [u8],
    separate: Option<&'a OsString>,
}

impl<'a> ValueOption<'a> {
    fn value(&self) -> &'a [u8] {
        self.separate.map_or(self.inline, |arg| arg.as_bytes())
    }
}

fn short_arguments(
    args: &[OsString],
) -> impl Iterator<Item = (&OsString, Option<ValueOption<'_>>)> {
    let mut args = args.iter();
    std::iter::from_fn(move || {
        let argument = args.next()?;
        let option = argument
            .as_bytes()
            .strip_prefix(b"-")
            .filter(|bytes| !bytes.starts_with(b"-"))
            .and_then(|flags| {
                let (index, flag) = flags
                    .iter()
                    .copied()
                    .enumerate()
                    .find(|(_, flag)| b"BbcDEeFIiJLlmOoPpQRSWw".contains(flag))?;
                let inline = &flags[index + 1..];
                let separate = if inline.is_empty() { args.next() } else { None };
                Some(ValueOption {
                    index,
                    flag,
                    inline,
                    separate,
                })
            });
        Some((argument, option))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewriting_jump_options_preserves_literal_values_and_grouped_flags() {
        let args = ["-P", "-J", "-vqJjump", "-oProxyJump=other", "-p", "22"].map(OsString::from);
        assert_eq!(
            without_proxy_arguments(&args),
            ["-P", "-J", "-vq", "-p", "22"]
        );
    }
}
