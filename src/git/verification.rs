use anyhow::{Context, Result};
use std::{
    env,
    ffi::{CString, OsStr, OsString},
    fs,
    io::{self, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{symlink, DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
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
const DELEGATE_SOURCE: &str = "GIT_ON_GNUPG_DELEGATE_SOURCE";
const DELEGATE_KEY: &str = "GIT_ON_GNUPG_DELEGATE_KEY";
const DELEGATE_PREFIX: &str = "d-";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    OpenPgp,
    X509,
}

impl Format {
    fn for_program(program: &str) -> Self {
        if program == "gpgsm" {
            Self::X509
        } else {
            Self::OpenPgp
        }
    }

    fn program(self) -> &'static str {
        match self {
            Self::OpenPgp => "gpg",
            Self::X509 => "gpgsm",
        }
    }
}

struct VerificationContext<'a> {
    cwd: &'a Path,
    source: Option<&'a Path>,
    caller: Option<PathBuf>,
    format: Option<Format>,
    openpgp_config: Option<OsString>,
}

impl<'a> VerificationContext<'a> {
    fn new(
        run: &Path,
        cwd: &'a Path,
        source: Option<&'a Path>,
        format: Option<Format>,
        program: &OsStr,
    ) -> Result<Self> {
        let openpgp_config = if format == Some(Format::X509) {
            None
        } else {
            select_openpgp_config(run, cwd, source, program)?
        };
        Ok(Self {
            cwd,
            source,
            caller: env::home_dir(),
            format,
            openpgp_config,
        })
    }

    fn key(&self) -> blake3::Hash {
        let mut hash = blake3::Hasher::new();
        if let Some(format) = self.format {
            hash.update(format.program().as_bytes());
        }
        hash.update(&[0]);
        if let Some(config) = &self.openpgp_config {
            hash.update(config.as_bytes());
        }
        hash.update(&[0]);
        for location in [Some(self.cwd), self.source, self.caller.as_deref()] {
            if let Some(location) = location {
                hash.update(location.as_os_str().as_bytes());
            }
            hash.update(&[0]);
        }
        hash.finalize()
    }

    fn bind_home(&self, command: &mut Command, home: &Path) {
        command
            .env("GNUPGHOME", home)
            .env("XDG_RUNTIME_DIR", home)
            .env(DELEGATE_HOME, home)
            .env(DELEGATE_KEY, self.key().to_hex().as_str())
            .env_remove(DELEGATE_SOURCE);
        if let Some(source) = self.source {
            command.env(DELEGATE_SOURCE, source);
        }
    }
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
            let cwd = command.get_current_dir().unwrap_or(Path::new("."));
            let source = resolve_home(cwd, env::var_os("GNUPGHOME"))?;
            let program = configured_program(config, "gpg.openpgp.program", "gpg")
                .unwrap_or(OsStr::new("gpg"));
            let context =
                VerificationContext::new(directory, cwd, source.as_deref(), None, program)?;
            let home = prepare_home(directory, &context, custom_verifier)?;
            command
                .env("GNUPGHOME", &home)
                .env("XDG_RUNTIME_DIR", &home);
            return Ok(());
        }
        let executable = std::env::current_exe()?;
        for (name, key, variable, default) in VERIFIERS {
            let configured = configured_program(config, key, default);
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

fn configured_program<'a>(
    config: &'a [(OsString, Option<OsString>)],
    key: &str,
    default: &str,
) -> Option<&'a OsStr> {
    config
        .iter()
        .rev()
        .find(|(setting, _)| setting == key || (default == "gpg" && setting == "gpg.program"))
        .and_then(|(_, value)| value.as_deref())
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
            let name = entry.file_name();
            if (name.as_bytes().starts_with(DELEGATE_PREFIX.as_bytes())
                || name.as_bytes().starts_with(b"delegate-"))
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
        let format = Format::for_program(default);
        let source = resolve_home(&cwd, env::var_os("GNUPGHOME"))?;
        let context =
            VerificationContext::new(run, &cwd, source.as_deref(), Some(format), &program)?;
        let home = prepare_home(run, &context, configured)?;
        let mut command = Command::new(program);
        command.args(arguments);
        let _delegate = if configured {
            Some(configure_delegate(&mut command, run, &home, &context)?)
        } else {
            command
                .env("GNUPGHOME", &home)
                .env("XDG_RUNTIME_DIR", &home);
            None
        };
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

fn resolve_program(cwd: &Path, program: &OsStr, path: &OsStr) -> Option<PathBuf> {
    let executable = |candidate: &Path| {
        candidate
            .metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    };
    if program.as_bytes().contains(&b'/') {
        let candidate = cwd.join(program);
        return executable(&candidate).then_some(candidate);
    }
    env::split_paths(path)
        .map(|path| cwd.join(path).join(program))
        .find(|candidate| executable(candidate))
}

fn configure_delegate(
    command: &mut Command,
    run: &Path,
    home: &Path,
    context: &VerificationContext<'_>,
) -> Result<tempfile::TempDir> {
    let original = env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"));
    let tools = tempfile::Builder::new()
        .prefix("tools-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(run)?;
    let executable = env::current_exe()?;
    for name in VERIFIER_TOOLS {
        let program = resolve_program(context.cwd, OsStr::new(name), &original);
        if let Some(program) = program {
            symlink(&executable, tools.path().join(name))?;
            command.env(verifier_program_variable(name), program);
        }
    }
    let path = env::join_paths(
        std::iter::once(tools.path().to_path_buf()).chain(env::split_paths(&original)),
    )?;
    command.env("PATH", path).env(DELEGATE_PATH, tools.path());
    context.bind_home(command, home);
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
    let mut command = Command::new(&program);
    command.args(forwarded);
    let existing = env::var_os(DELEGATE_HOME).context("missing private GnuPG home")?;
    let inherited_home = source.as_deref() == Some(existing.as_os_str());
    let format = Format::for_program(name);
    let cwd = env::current_dir()?;
    let source = if inherited_home {
        env::var_os(DELEGATE_SOURCE).map(PathBuf::from)
    } else {
        resolve_home(&cwd, source)?
    };
    let context = VerificationContext::new(run, &cwd, source.as_deref(), Some(format), &program)?;
    let key = context.key().to_hex();
    let same_context = env::var_os(DELEGATE_KEY).as_deref() == Some(OsStr::new(key.as_str()));
    let home = if inherited_home && same_context {
        PathBuf::from(existing)
    } else {
        delegated_home(run, &context)?
    };
    context.bind_home(&mut command, &home);
    Ok(command.status()?)
}

fn delegated_home(run: &Path, context: &VerificationContext<'_>) -> Result<PathBuf> {
    // Keep the socket pathname inside GnuPG's Unix socket length limit.
    let key = context.key().to_hex();
    let directory = run.join(format!("{DELEGATE_PREFIX}{}", &key[..16]));
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&directory)?;
    let marker = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(directory.join("prepare.lock"))?;
    marker.lock()?;
    let home = directory.join("keys");
    let ready = directory.join("prepared");
    if !ready.try_exists()? {
        // An interrupted preparation can leave a partial copy before it
        // releases the lock. Only expose the home after the complete copy.
        match fs::remove_dir_all(&home).or_else(|error| {
            if error.kind() == io::ErrorKind::NotADirectory {
                fs::remove_file(&home)
            } else {
                Err(error)
            }
        }) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        prepare_home(&directory, context, true)?;
        fs::File::create(ready)?;
    }
    Ok(home)
}

fn resolve_home(cwd: &Path, source_home: Option<OsString>) -> Result<Option<PathBuf>> {
    Ok(match source_home.filter(|source| !source.is_empty()) {
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
        None => env::home_dir().map(|caller| caller.join(".gnupg")),
    })
}

fn versioned_openpgp_config(name: &[u8]) -> bool {
    name.strip_prefix(b"gpg.conf-")
        .and_then(|version| version.first())
        .is_some_and(u8::is_ascii_digit)
}

fn has_access(path: &Path, mode: libc::c_int) -> io::Result<bool> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    Ok(unsafe { libc::access(path.as_ptr(), mode) } == 0)
}

fn list_home(path: &Path) -> io::Result<Option<fs::ReadDir>> {
    match fs::read_dir(path) {
        Ok(entries) => Ok(Some(entries)),
        // An unlistable directory can still allow lookup of known files.
        Err(error)
            if error.kind() == io::ErrorKind::PermissionDenied && has_access(path, libc::X_OK)? =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn select_openpgp_config(
    run: &Path,
    cwd: &Path,
    source: Option<&Path>,
    program: &OsStr,
) -> Result<Option<OsString>> {
    let default = OsString::from("gpg.conf");
    let Some(source) = source else {
        return Ok(Some(default));
    };
    let entries = match list_home(source) {
        Ok(entries) => entries,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound
                    | io::ErrorKind::NotADirectory
                    | io::ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(Some(default));
        }
        Err(error) => return Err(error.into()),
    };
    let can_list = entries.is_some();
    if let Some(entries) = entries {
        let mut has_versioned_config = false;
        for entry in entries {
            let entry = entry?;
            if versioned_openpgp_config(entry.file_name().as_bytes())
                && has_access(&entry.path(), libc::R_OK)?
            {
                has_versioned_config = true;
                break;
            }
        }
        if !has_versioned_config {
            return Ok(Some(default));
        }
    }

    let shadow = tempfile::Builder::new()
        .prefix("c-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(run)?;
    let Some(backend) = native_openpgp_backend(program, cwd, shadow.path(), !can_list)? else {
        return Ok(None);
    };
    let result = Command::new(backend)
        .current_dir(cwd)
        .env("GNUPGHOME", shadow.path())
        .env("XDG_RUNTIME_DIR", shadow.path())
        .env("TMPDIR", shadow.path())
        .args(["--no-options", "--with-colons", "--list-config", "version"])
        .stdin(Stdio::null())
        .output()?;
    let Some(version) = result
        .status
        .success()
        .then(|| std::str::from_utf8(&result.stdout).ok())
        .flatten()
        .and_then(|output| {
            output
                .lines()
                .find_map(|line| line.strip_prefix("cfg:version:"))
        })
        .filter(|version| {
            version.starts_with(|c: char| c.is_ascii_digit())
                && version
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b".-_+".contains(&byte))
        })
    else {
        return Ok(None);
    };
    // GnuPG tries the full version, then removes each suffix separated by a
    // dot or dash. Looking up these names also works without directory listing.
    let lengths =
        std::iter::once(version.len()).chain(version.rmatch_indices(['.', '-']).map(|(i, _)| i));
    for len in lengths {
        let name = format!("gpg.conf-{}", &version[..len]);
        if has_access(&source.join(&name), libc::R_OK)? {
            return Ok(Some(name.into()));
        }
    }
    Ok(Some(default))
}

fn native_openpgp_backend(
    program: &OsStr,
    cwd: &Path,
    scratch: &Path,
    use_default_for_unlistable_home: bool,
) -> Result<Option<PathBuf>> {
    let path = env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"));
    let result = match Command::new("gpgconf")
        .current_dir(cwd)
        .env("GNUPGHOME", scratch)
        .env("XDG_RUNTIME_DIR", scratch)
        .env("TMPDIR", scratch)
        .args(["--list-dirs", "bindir"])
        .stdin(Stdio::null())
        .output()
    {
        Ok(result) if result.status.success() => result,
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Some(directory) = result.stdout.strip_suffix(b"\n") else {
        return Ok(None);
    };
    let directory = Path::new(OsStr::from_bytes(directory));
    if !directory.is_absolute() {
        return Ok(None);
    }
    let backend = directory.join("gpg");
    let Ok(actual) = backend.metadata() else {
        return Ok(None);
    };
    // Never execute a configured wrapper to discover a GnuPG version. With
    // an unlistable home, use the installed PATH delegate for known filenames.
    let programs = std::iter::once(program)
        .chain(use_default_for_unlistable_home.then_some(OsStr::new("gpg")));
    let matches_installation = programs
        .filter_map(|program| resolve_program(cwd, program, &path))
        .filter_map(|program| program.metadata().ok())
        .any(|expected| (actual.dev(), actual.ino()) == (expected.dev(), expected.ino()));
    Ok(matches_installation.then_some(backend))
}

fn prepare_home(
    run: &Path,
    context: &VerificationContext<'_>,
    copy_extra: bool,
) -> Result<PathBuf> {
    let home = run.join("keys");
    let mut source = context.source;
    let mut entries = match source.map(list_home).transpose() {
        Ok(entries) => entries.flatten(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            source = None;
            None
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotADirectory | io::ErrorKind::PermissionDenied
            ) =>
        {
            // Preserve an unusable inherited home in private storage. A
            // configured wrapper can still select a different home.
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&home)?;
            return Ok(home);
        }
        Err(error) => return Err(error.into()),
    };
    match fs::DirBuilder::new().mode(0o700).create(&home) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(home),
        Err(error) => return Err(error.into()),
    }
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
    let mut directories = Vec::new();
    if entries.is_some() && (copy_extra || context.format != Some(Format::OpenPgp)) {
        let metadata = fs::metadata(source)?;
        directories.push((metadata.dev(), metadata.ino()));
    }
    let mut named_files = 0;
    let other_options = [
        "gpgsm.conf",
        "gpg-agent.conf",
        "common.conf",
        "keyboxd.conf",
        "dirmngr.conf",
    ]
    .map(OsStr::new);
    let default = OsStr::new("gpg.conf");
    let selected = context.openpgp_config.as_deref().unwrap_or(default);
    let alternate = (entries.is_none() && copy_extra && selected != default).then_some(default);
    let mut known = std::iter::once(selected)
        .chain(alternate)
        .chain(other_options)
        .chain(std::iter::once(OsStr::new("crls.d")));
    loop {
        let name = match &mut entries {
            Some(entries) => entries.next().transpose()?.map(|entry| entry.file_name()),
            None => known.next().map(OsString::from),
        };
        let Some(name) = name else { break };
        let filename = name.as_bytes();
        let openpgp = filename == b"gpg.conf" || versioned_openpgp_config(filename);
        let explicit_default = alternate == Some(name.as_os_str());
        // Git dispatches verification by the signature format. Each GnuPG
        // verifier reads its own options, plus the shared helper configs.
        if (context.format == Some(Format::OpenPgp)
            && matches!(filename, b"gpgsm.conf" | b"crls.d"))
            || (openpgp
                && (context.format == Some(Format::X509)
                    || context
                        .openpgp_config
                        .as_deref()
                        .is_some_and(|selected| name != selected && !explicit_default)))
        {
            continue;
        }
        if !openpgp && !other_options.contains(&name.as_os_str()) {
            if copy_extra || filename == b"crls.d" {
                copy_verifier_input(source.join(&name), &home.join(name), &mut directories)?;
            }
            continue;
        }
        let original = source.join(&name);
        let defer_selection = explicit_default || (openpgp && context.openpgp_config.is_none());
        if defer_selection && !has_access(&original, libc::R_OK)? {
            continue;
        }
        let target = home.join(&name);
        let result = copy_options(
            &original,
            &target,
            source,
            context.cwd,
            context.caller.as_deref(),
            &mut named_files,
        );
        if let Err(error) = result {
            if !defer_selection {
                return Err(error);
            }
            // A wrapper can select a versioned file itself. Report staging
            // errors only if its GnuPG parser actually reads that file.
            fs::write(&target, b"git-on-could-not-prepare-verification-input\n")?;
        }
    }
    Ok(home)
}

fn copy_options(
    original: &Path,
    target: &Path,
    source: &Path,
    cwd: &Path,
    caller: Option<&Path>,
    named_files: &mut usize,
) -> Result<()> {
    let Some(config) = read_public_file(original)? else {
        return Ok(());
    };
    let home = target
        .parent()
        .context("private verifier options directory")?;
    let mut output = fs::File::create(target)?;
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
                        .find_map(|prefix| value.strip_prefix(prefix).map(|path| (path, prefix)))
                        .unwrap_or((value, &[]))
                };
                let path = if policy {
                    // gpgsm opens an explicit policy from its working directory.
                    cwd.join(OsStr::from_bytes(path))
                } else if let Some(suffix) = path.strip_prefix(b"~/") {
                    caller
                        .context("resolve caller's GnuPG path")?
                        .join(OsStr::from_bytes(suffix))
                } else if path.starts_with(b"~") && path.contains(&b'/') {
                    resolve_home(cwd, Some(OsString::from_vec(path.to_vec())))?
                        .context("resolve named user's GnuPG path")?
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
                *named_files += 1;
                let destination = home.join(&private);
                copy_public_file(&path, &destination)?;
                output.write_all(option)?;
                output.write_all(b" ")?;
                output.write_all(prefix)?;
                // A wrapper may pass this options file to a delegate with
                // another private home. The file still refers to this input.
                output.write_all(destination.as_os_str().as_bytes())?;
                output.write_all(b"\n")?;
            }
            _ => output.write_all(line)?,
        }
    }
    Ok(())
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
    source: PathBuf,
    target: &Path,
    directories: &mut Vec<(u64, u64)>,
) -> Result<()> {
    if source.file_name().is_some_and(excluded_verifier_input) {
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(&source) {
        Ok(metadata) => metadata,
        Err(error) if optional_verifier_input(&error) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let (source, metadata) = if metadata.is_symlink() {
        let resolved = match fs::canonicalize(&source) {
            Ok(path) => path,
            Err(error)
                if optional_verifier_input(&error) || error.raw_os_error() == Some(libc::ELOOP) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        if resolved
            .components()
            .any(|part| excluded_verifier_input(part.as_os_str()))
        {
            return Ok(());
        }
        match fs::metadata(&resolved) {
            Ok(metadata) => (resolved, metadata),
            Err(error) if optional_verifier_input(&error) => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    } else {
        (source, metadata)
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
            copy_verifier_input(child.path(), &target.join(child.file_name()), directories)?;
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
    fn delegated_configuration_resolves_relative_policy_in_each_working_directory() -> Result<()> {
        let scratch = tempfile::tempdir()?;
        let source = tempfile::tempdir()?;
        fs::write(
            source.path().join("gpgsm.conf"),
            b"policy-file issuer.policy\n",
        )?;
        for value in ["1.2.3.4", "1.2.3.5"] {
            let cwd = tempfile::tempdir()?;
            fs::write(cwd.path().join("issuer.policy"), value)?;
            let home = delegated_home(
                scratch.path(),
                &VerificationContext {
                    cwd: cwd.path(),
                    source: Some(source.path()),
                    caller: None,
                    format: Some(Format::X509),
                    openpgp_config: Some("gpg.conf".into()),
                },
            )?;
            let config = fs::read_to_string(home.join("gpgsm.conf"))?;
            let path = config
                .strip_prefix("policy-file ")
                .context("prepared policy")?;
            assert_eq!(fs::read_to_string(path.trim_end())?, value);
        }
        Ok(())
    }

    #[test]
    fn delegated_keyring_resolves_tilde_in_each_callers_home() -> Result<()> {
        let scratch = tempfile::tempdir()?;
        let source = tempfile::tempdir()?;
        fs::write(source.path().join("gpg.conf"), b"keyring ~/public.gpg\n")?;
        for data in [b"first", b"other"] {
            let caller = tempfile::tempdir()?;
            fs::write(caller.path().join("public.gpg"), data)?;
            let home = delegated_home(
                scratch.path(),
                &VerificationContext {
                    cwd: scratch.path(),
                    source: Some(source.path()),
                    caller: Some(caller.path().to_path_buf()),
                    format: Some(Format::OpenPgp),
                    openpgp_config: Some("gpg.conf".into()),
                },
            )?;
            let config = fs::read_to_string(home.join("gpg.conf"))?;
            let path = config
                .strip_prefix("keyring ")
                .context("prepared keyring")?;
            assert_eq!(fs::read(home.join(path.trim_end()))?, data);
        }
        Ok(())
    }

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
            &VerificationContext {
                cwd: source.path(),
                source: Some(source.path()),
                caller: None,
                format: Some(Format::X509),
                openpgp_config: None,
            },
            false,
        )?;
        assert_eq!(
            fs::read(private.join("crls.d/DIR.txt"))?,
            b"native cache index"
        );
        assert!(!private.join("crls.d/cache.lock").exists());
        Ok(())
    }
}
