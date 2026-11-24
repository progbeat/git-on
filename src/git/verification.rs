use anyhow::{Context, Result};
use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{self, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{symlink, DirBuilderExt, MetadataExt, PermissionsExt},
        process::ExitStatusExt,
    },
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::atomic::{AtomicBool, Ordering},
};

static ENABLED: AtomicBool = AtomicBool::new(false);
const VERIFIER_TOOLS: &[&str] = &["gpg", "gpg2", "gpgsm"];
const DELEGATE_PATH: &str = "GIT_ON_GNUPG_DELEGATE_PATH";
const DELEGATE_HOME: &str = "GIT_ON_GNUPG_DELEGATE_HOME";
const DELEGATE_PREFIX: &str = "delegate-";

#[derive(Clone, Copy)]
enum VerificationInputs {
    OpenPgp,
    X509,
    Configured,
}

const VERIFIERS: [(&str, &str, &str, &str); 2] = [
    (
        "git-on-openpgp",
        "gpg.openpgp.program",
        "GIT_ON_OPENPGP_PROGRAM",
        "gpg",
    ),
    (
        "git-on-x509",
        "gpg.x509.program",
        "GIT_ON_X509_PROGRAM",
        "gpgsm",
    ),
];

pub(super) struct Scratch(crate::service_memory::Directory);

impl Scratch {
    pub(super) fn new() -> Result<Self> {
        Ok(Self(crate::service_memory::Directory::new("v-")?))
    }

    pub(super) fn configure(
        &self,
        command: &mut Command,
        config: &[(OsString, Option<OsString>)],
    ) -> Result<()> {
        let directory = self.0.path();
        command.env("TMPDIR", directory);
        if !ENABLED.load(Ordering::Relaxed) {
            // Library callers may use their own executable without the helper entrypoint.
            let custom_verifier = config.iter().any(|(key, value)| {
                value.is_some()
                    && matches!(
                        key.as_bytes(),
                        b"gpg.program" | b"gpg.openpgp.program" | b"gpg.x509.program"
                    )
            });
            let home = prepare_home(
                directory,
                command.get_current_dir().unwrap_or(Path::new(".")),
                if custom_verifier {
                    VerificationInputs::Configured
                } else {
                    VerificationInputs::X509
                },
                env::var_os("GNUPGHOME"),
            )?;
            command
                .env("GNUPGHOME", &home)
                .env("XDG_RUNTIME_DIR", &home);
            return Ok(());
        }
        let executable = std::env::current_exe()?;
        for (name, key, variable, default) in VERIFIERS {
            let configured = config
                .iter()
                .rev()
                .find(|(setting, _)| {
                    setting == key || (default == "gpg" && setting == "gpg.program")
                })
                .and_then(|(_, value)| value.as_deref());
            let helper = directory.join(name);
            symlink(&executable, &helper)?;
            let mut option = OsString::from(format!("{key}="));
            option.push(helper);
            command
                .arg("-c")
                .arg(option)
                .env(variable, configured.unwrap_or(OsStr::new(default)))
                .env(
                    format!("{variable}_CONFIGURED"),
                    if configured.is_some() { "1" } else { "0" },
                );
        }
        Ok(())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        stop_workspace_verifiers(self.0.path());
    }
}

pub(crate) fn stop_memory_verifiers(root: &Path) {
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            if entry.file_name().as_bytes().starts_with(b"v-")
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
            {
                stop_workspace_verifiers(&entry.path());
            }
        }
    }
}

fn stop_workspace_verifiers(directory: &Path) {
    stop_verifiers(directory);
    if let Ok(entries) = fs::read_dir(directory) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .as_bytes()
                .starts_with(DELEGATE_PREFIX.as_bytes())
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
            {
                stop_verifiers(&entry.path());
            }
        }
    }
}

fn stop_verifiers(directory: &Path) {
    let home = directory.join("keys");
    if ["S.keyboxd", "S.dirmngr", "S.gpg-agent"]
        .iter()
        .any(|socket| home.join(socket).exists())
    {
        let _ = Command::new("gpgconf")
            .arg("--homedir")
            .arg(&home)
            .args(["--kill", "all"])
            .env("XDG_RUNTIME_DIR", &home)
            .env("TMPDIR", directory)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

pub fn invoke_verifier() -> Option<i32> {
    ENABLED.store(true, Ordering::Relaxed);
    let mut arguments = env::args_os();
    let entry = PathBuf::from(arguments.next()?);
    if let Some(path) = env::var_os(DELEGATE_PATH).map(PathBuf::from) {
        let filename = entry.file_name()?;
        if entry == Path::new(filename) || entry.parent() == Some(path.as_path()) {
            let name = filename.to_str()?;
            if VERIFIER_TOOLS.contains(&name) {
                return Some(verifier_status(run_delegate(
                    name,
                    arguments,
                    path.parent()?,
                )));
            }
        }
    }
    let (_, _, variable, default) = VERIFIERS
        .iter()
        .find(|(name, _, _, _)| entry.file_name() == Some(OsStr::new(name)))?;
    let program = std::env::var_os(variable)?;
    let run = entry.parent()?;
    Some(verifier_status((|| -> Result<_> {
        let configured =
            std::env::var_os(format!("{variable}_CONFIGURED")).as_deref() == Some(OsStr::new("1"));
        let cwd = env::current_dir()?;
        let inputs = if configured {
            VerificationInputs::Configured
        } else if *default == "gpgsm" {
            VerificationInputs::X509
        } else {
            VerificationInputs::OpenPgp
        };
        let home = prepare_home(run, &cwd, inputs, env::var_os("GNUPGHOME"))?;
        let mut command = Command::new(program);
        command
            .args(arguments)
            .env("GNUPGHOME", &home)
            .env("XDG_RUNTIME_DIR", &home);
        let _delegate = configured
            .then(|| configure_delegate(&mut command, run, &cwd, &home))
            .transpose()?;
        Ok(command.status()?)
    })()))
}

fn verifier_status(outcome: Result<ExitStatus>) -> i32 {
    match outcome {
        Ok(status) => status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)),
        Err(error) => {
            let _ = writeln!(
                std::io::stderr().lock(),
                "git-on: prepare native signature verification: {error:#}"
            );
            125
        }
    }
}

fn verifier_program_variable(name: &str) -> String {
    format!("GIT_ON_NATIVE_GNUPG_{name}")
}

fn configure_delegate(
    command: &mut Command,
    run: &Path,
    cwd: &Path,
    home: &Path,
) -> Result<tempfile::TempDir> {
    let original = env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"));
    let tools = tempfile::Builder::new()
        .prefix("tools-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(run)?;
    let executable = env::current_exe()?;
    for name in VERIFIER_TOOLS {
        let program = env::split_paths(&original)
            .map(|path| cwd.join(path).join(name))
            .find(|candidate| {
                candidate
                    .metadata()
                    .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            });
        if let Some(program) = program {
            symlink(&executable, tools.path().join(name))?;
            command.env(verifier_program_variable(name), program);
        }
    }
    let path = env::join_paths(
        std::iter::once(tools.path().to_path_buf()).chain(env::split_paths(&original)),
    )?;
    command
        .env("PATH", path)
        .env(DELEGATE_PATH, tools.path())
        .env(DELEGATE_HOME, home);
    Ok(tools)
}

fn run_delegate(
    name: &str,
    mut arguments: impl Iterator<Item = OsString>,
    run: &Path,
) -> Result<ExitStatus> {
    let program =
        env::var_os(verifier_program_variable(name)).context("missing native GnuPG executable")?;
    let mut source = env::var_os("GNUPGHOME");
    let mut forwarded = Vec::new();
    while let Some(argument) = arguments.next() {
        if argument == "--homedir" {
            source = Some(arguments.next().context("GnuPG --homedir needs a path")?);
        } else if let Some(home) = argument.as_bytes().strip_prefix(b"--homedir=") {
            source = Some(OsString::from_vec(home.to_vec()));
        } else {
            let boundary = argument == "--";
            forwarded.push(argument);
            if boundary {
                forwarded.extend(arguments);
                break;
            }
        }
    }
    let mut command = Command::new(program);
    command.args(forwarded);
    let existing = env::var_os(DELEGATE_HOME).context("missing private GnuPG home")?;
    if source.as_deref() == Some(existing.as_os_str()) {
        return Ok(command
            .env("GNUPGHOME", &existing)
            .env("XDG_RUNTIME_DIR", &existing)
            .status()?);
    }
    let scratch = tempfile::Builder::new()
        .prefix(DELEGATE_PREFIX)
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(run)?;
    let home = prepare_home(
        scratch.path(),
        &env::current_dir()?,
        VerificationInputs::Configured,
        source,
    )?;
    let status = command
        .env("GNUPGHOME", &home)
        .env("XDG_RUNTIME_DIR", &home)
        .status();
    stop_verifiers(scratch.path());
    Ok(status?)
}

fn prepare_home(
    run: &Path,
    cwd: &Path,
    inputs: VerificationInputs,
    source_home: Option<OsString>,
) -> Result<PathBuf> {
    let home = run.join("keys");
    match fs::DirBuilder::new().mode(0o700).create(&home) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(home),
        Err(error) => return Err(error.into()),
    }
    let caller = std::env::home_dir();
    let source = match source_home.filter(|source| !source.is_empty()) {
        Some(source) if source.as_bytes().starts_with(b"~") => {
            // Ask GnuPG to resolve both ~/ and ~user/ with its own home rules.
            let output = Command::new("gpgconf")
                .current_dir(cwd)
                .env("GNUPGHOME", source)
                .args(["--list-dirs", "homedir"])
                .stdin(Stdio::null())
                .output()
                .context("resolve GnuPG home")?;
            anyhow::ensure!(
                output.status.success(),
                "resolve GnuPG home: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            let path = output
                .stdout
                .strip_suffix(b"\n")
                .filter(|path| !path.is_empty())
                .context("GnuPG did not report a home directory")?;
            Some(PathBuf::from(OsString::from_vec(path.to_vec())))
        }
        Some(source) => Some(cwd.join(source)),
        None => caller.as_ref().map(|caller| caller.join(".gnupg")),
    };
    let Some(source) = source else {
        return Ok(home);
    };
    for name in [
        "pubring.gpg",
        "pubring.kbx",
        "trustdb.gpg",
        "tofu.db",
        "trustlist.txt",
        "policies.txt",
        "qualified.txt",
        "public-keys.d/pubring.db",
        "public-keys.d/pubring.db-wal",
    ] {
        copy_public_file(&source.join(name), &home.join(name))?;
    }
    let entries = match fs::read_dir(&source) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(home),
        Err(error) => return Err(error.into()),
    };
    let mut directories = Vec::new();
    if !matches!(inputs, VerificationInputs::OpenPgp) {
        let metadata = fs::metadata(&source)?;
        directories.push((metadata.dev(), metadata.ino()));
    }
    let mut named_files = 0;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let filename = name.as_bytes();
        // Preserve version-qualified names so GnuPG chooses the applicable file.
        let versioned = filename
            .strip_prefix(b"gpg.conf-")
            .and_then(|version| version.first())
            .is_some_and(u8::is_ascii_digit);
        if !versioned
            && !matches!(
                filename,
                b"gpg.conf"
                    | b"gpgsm.conf"
                    | b"gpg-agent.conf"
                    | b"common.conf"
                    | b"keyboxd.conf"
                    | b"dirmngr.conf"
            )
        {
            if matches!(inputs, VerificationInputs::Configured)
                || matches!(inputs, VerificationInputs::X509) && filename == b"crls.d"
            {
                copy_verifier_input(&entry, &home.join(name), &mut directories)?;
            }
            continue;
        }
        let config = match read_public_file(&entry.path()) {
            Ok(Some(config)) => config,
            Ok(None) => continue,
            Err(error)
                if versioned
                    && error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                        matches!(
                            error.kind(),
                            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
                        )
                    }) =>
            {
                // GnuPG chooses its first readable versioned configuration.
                continue;
            }
            Err(error) => return Err(error),
        };
        let mut output = fs::File::create(home.join(&name))?;
        for line in config.split_inclusive(|byte| *byte == b'\n') {
            let trimmed = line.trim_ascii_start();
            let end = trimmed
                .iter()
                .position(|byte| byte.is_ascii_whitespace())
                .unwrap_or(trimmed.len());
            let (option, parameters) = trimmed.split_at(end);
            let option = option.strip_prefix(b"--").unwrap_or(option);
            // GnuPG's "options" directive selects a file only on the command line.
            match option {
                b"log-file"
                | b"logger-file"
                | b"debug-log-file"
                | b"status-file"
                | b"logger-fd"
                | b"status-fd"
                | b"attribute-fd"
                | b"attribute-file"
                | b"output"
                | b"write-env-file"
                | b"extra-socket"
                | b"browser-socket"
                | b"pinentry-touch-file" => continue,
                b"keyring" | b"primary-keyring" | b"trustdb-name" | b"policy-file" => {
                    let value = parameters.trim_ascii();
                    // GnuPG removes an opening double quote and an optional
                    // closing double quote after trimming the option value.
                    let value = value
                        .strip_prefix(b"\"")
                        .map_or(value, |quoted| quoted.strip_suffix(b"\"").unwrap_or(quoted));
                    if value.is_empty() {
                        output.write_all(line)?;
                        continue;
                    }
                    let policy = option == b"policy-file";
                    let (path, prefix) = if policy {
                        (value, &[][..])
                    } else {
                        [b"gnupg-kbx:".as_slice(), b"gnupg-ring:"]
                            .into_iter()
                            .find_map(|prefix| {
                                value.strip_prefix(prefix).map(|path| (path, prefix))
                            })
                            .unwrap_or((value, &[]))
                    };
                    let path = if policy {
                        // gpgsm opens an explicit policy from its working directory.
                        cwd.join(OsStr::from_bytes(path))
                    } else if let Some(suffix) = path.strip_prefix(b"~/") {
                        caller
                            .as_deref()
                            .context("resolve caller's GnuPG path")?
                            .join(OsStr::from_bytes(suffix))
                    } else if path.contains(&b'/') {
                        cwd.join(OsStr::from_bytes(path))
                    } else {
                        source.join(OsStr::from_bytes(path))
                    };
                    let extension = if path.extension() == Some(OsStr::new("kbx")) {
                        "kbx"
                    } else {
                        "gpg"
                    };
                    let private = format!("input-{named_files}.{extension}");
                    named_files += 1;
                    let destination = home.join(&private);
                    copy_public_file(&path, &destination)?;
                    output.write_all(option)?;
                    output.write_all(b" ")?;
                    output.write_all(prefix)?;
                    let filename = if policy {
                        destination.as_os_str().as_bytes()
                    } else {
                        private.as_bytes()
                    };
                    output.write_all(filename)?;
                    output.write_all(b"\n")?;
                }
                _ => output.write_all(line)?,
            }
        }
    }
    Ok(home)
}

fn public_file(path: &Path) -> Result<bool> {
    match fs::metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.is_file(),
                "GnuPG input is not a regular file: {}",
                path.display()
            );
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("read GnuPG input {}", path.display())),
    }
}

fn read_public_file(path: &Path) -> Result<Option<Vec<u8>>> {
    public_file(path)?
        .then(|| fs::read(path))
        .transpose()
        .map_err(Into::into)
}

fn copy_public_file(source: &Path, target: &Path) -> Result<()> {
    if public_file(source)? {
        copy_verifier_file(source, target).context("copy public GnuPG state for verification")?;
    }
    Ok(())
}

fn excluded_verifier_input(name: &OsStr) -> bool {
    matches!(
        name.as_bytes(),
        b"private-keys-v1.d" | b"openpgp-revocs.d" | b"secring.gpg" | b"random_seed"
    ) || name.as_bytes().starts_with(b".#lk")
        || name.as_bytes().ends_with(b".lock")
}

// Configured verifiers may open relative paths inside their home. Copy
// regular inputs at those paths and leave caller keys, sockets and locks.
fn copy_verifier_input(
    entry: &fs::DirEntry,
    target: &Path,
    directories: &mut Vec<(u64, u64)>,
) -> Result<()> {
    if excluded_verifier_input(&entry.file_name()) {
        return Ok(());
    }
    let mut source = entry.path();
    if entry.file_type()?.is_symlink() {
        source = match fs::canonicalize(&source) {
            Ok(path) => path,
            Err(error)
                if optional_verifier_input(&error) || error.raw_os_error() == Some(libc::ELOOP) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        if source
            .components()
            .any(|part| excluded_verifier_input(part.as_os_str()))
        {
            return Ok(());
        }
    }
    let metadata = match fs::metadata(&source) {
        Ok(metadata) => metadata,
        Err(error) if optional_verifier_input(&error) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.is_dir() {
        let identity = (metadata.dev(), metadata.ino());
        if directories.contains(&identity) {
            return Ok(());
        }
        let children = match fs::read_dir(&source) {
            Ok(children) => children,
            Err(error) if optional_verifier_input(&error) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        directories.push(identity);
        for child in children {
            let child = child?;
            copy_verifier_input(&child, &target.join(child.file_name()), directories)?;
        }
        directories.pop();
    } else if metadata.is_file() && !target.try_exists()? {
        if let Err(error) = copy_verifier_file(&source, target) {
            if !optional_verifier_input(&error) {
                return Err(error.into());
            }
        }
    }
    Ok(())
}

fn optional_verifier_input(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
    )
}

fn copy_verifier_file(source: &Path, target: &Path) -> io::Result<()> {
    fs::create_dir_all(target.parent().unwrap())?;
    fs::copy(source, target)?;
    fs::set_permissions(target, fs::Permissions::from_mode(0o600))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x509_preparation_copies_the_crl_cache_without_its_locks() -> Result<()> {
        let source = tempfile::tempdir()?;
        let scratch = tempfile::tempdir()?;
        let cache = source.path().join("crls.d");
        fs::create_dir(&cache)?;
        fs::write(cache.join("DIR.txt"), b"native cache index")?;
        fs::write(cache.join("cache.lock"), b"caller lock")?;
        let private = prepare_home(
            scratch.path(),
            source.path(),
            VerificationInputs::X509,
            Some(source.path().as_os_str().to_owned()),
        )?;
        assert_eq!(
            fs::read(private.join("crls.d/DIR.txt"))?,
            b"native cache index"
        );
        assert!(!private.join("crls.d/cache.lock").exists());
        Ok(())
    }
}
