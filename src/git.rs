//! Native Git configuration and signature policy. No replacement objects.
use anyhow::{bail, Context, Result};
use git2::{ObjectType, Oid};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{self, BufRead, Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub(crate) mod diagnostics;
mod index;
#[cfg(test)]
pub(crate) use diagnostics::full_output as full_test_output;

#[derive(Debug, Clone)]
pub struct Git {
    pub cwd: PathBuf,
    config: Vec<(OsString, Option<OsString>)>,
    original_dir: Option<PathBuf>,
    original_worktree: Option<PathBuf>,
}
impl Git {
    pub fn capture(cwd: &Path) -> Result<Self> {
        let mut git = Self::capture_configuration(cwd)?;
        git.original_dir = git.caller_path(&["--absolute-git-dir"]);
        if git.original_dir.is_some() {
            git.original_worktree = git.caller_path(&["--show-toplevel"]);
        }
        Ok(git)
    }

    fn caller_path(&self, options: &[&str]) -> Option<PathBuf> {
        let mut command = native_git_command();
        command
            .current_dir(&self.cwd)
            .arg("rev-parse")
            .args(options)
            .stdin(Stdio::null());
        run_builtin(command)
            .ok()
            .filter(|output| output.status.success())
            .map(|mut output| {
                if output.stdout.last() == Some(&b'\n') {
                    output.stdout.pop();
                }
                PathBuf::from(OsString::from_vec(output.stdout))
            })
    }

    pub(crate) fn capture_configuration(cwd: &Path) -> Result<Self> {
        let mut config = Vec::new();
        let output = read_configuration(cwd, &["--includes"])?;
        for record in output.split(|b| *b == 0).filter(|x| !x.is_empty()) {
            let (key, value) = record.split_once_byte(b'\n');
            // Includes were expanded in the caller's original repository context.
            if is_config_include(key) {
                continue;
            }
            config.push((
                OsString::from_vec(key.to_vec()),
                value.map(|v| OsString::from_vec(v.to_vec())),
            ));
        }
        Ok(Self {
            cwd: cwd.to_path_buf(),
            config,
            original_dir: None,
            original_worktree: None,
        })
    }

    pub(crate) fn capture_snapshot(cwd: &Path, revision: &OsStr) -> Result<(Self, Oid)> {
        let mut git = Self {
            cwd: cwd.to_path_buf(),
            config: Vec::new(),
            original_dir: None,
            original_worktree: None,
        };
        if index::selects_from_index(revision) {
            let (directory, oid) = index::resolve(&git, revision)?;
            git.original_dir = Some(directory);
            return Ok((git, oid));
        }
        let output = git.output(
            None,
            &[
                OsStr::new("rev-parse"),
                OsStr::new("--absolute-git-dir"),
                OsStr::new("--verify"),
                OsStr::new("--end-of-options"),
                revision,
            ],
        )?;
        // The repository path can contain a newline. The last line is the OID.
        let mut fields = output
            .strip_suffix(b"\n")
            .unwrap_or(&output)
            .rsplitn(2, |&byte| byte == b'\n');
        let oid = Oid::from_str(std::str::from_utf8(fields.next().unwrap_or_default())?)
            .context("resolve exact SHA-1 revision (SHA-256 repositories unsupported)")?;
        let path_len = fields
            .next()
            .context("native Git did not identify the selected repository directory")?
            .len();
        let mut path = output;
        path.truncate(path_len);
        git.original_dir = Some(PathBuf::from(OsString::from_vec(path)));
        Ok((git, oid))
    }
    pub fn command(&self, dir: Option<&Path>) -> Command {
        let mut command = native_git_command();
        command
            .current_dir(&self.cwd)
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_NO_LAZY_FETCH", "1")
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
            ]);
        if let Some(dir) = dir {
            self.configure_repository(&mut command, dir);
        }
        if let Some(program) = self.config.iter().rev().find_map(|(key, value)| {
            (key == "core.askpass")
                .then_some(value.as_deref())
                .flatten()
        }) {
            crate::terminal_ui::core_askpass(&mut command, program);
        }
        command
    }

    fn configure_repository(&self, command: &mut Command, dir: &Path) {
        if self.original_dir.as_deref() != Some(dir) {
            // Replay configuration from the original invocation. The private
            // repository supplies its own object and ref storage format.
            command
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env_remove("GIT_CONFIG_PARAMETERS")
                .env_remove("GIT_CONFIG_COUNT");
            let mut count = 0;
            for (key, value) in &self.config {
                if key == "core.repositoryformatversion"
                    || key.as_bytes().starts_with(b"extensions.")
                {
                    continue;
                }
                let value = value.as_deref().unwrap_or(OsStr::new("true"));
                let variable = format!("GIT_CONFIG_VALUE_{count}");
                command.env(format!("GIT_CONFIG_KEY_{count}"), key);
                command.env(variable, value);
                count += 1;
            }
            command.env("GIT_CONFIG_COUNT", count.to_string());
            // Local upload-pack inherits GIT_NAMESPACE from native Git.
            for key in [
                "GIT_DIR",
                "GIT_WORK_TREE",
                "GIT_COMMON_DIR",
                "GIT_OBJECT_DIRECTORY",
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                "GIT_INDEX_FILE",
                "GIT_SHALLOW_FILE",
                "GIT_GRAFT_FILE",
                "GIT_QUARANTINE_PATH",
            ] {
                command.env_remove(key);
            }
        }
        command
            .arg("--git-dir")
            .arg(dir)
            .args(["-c", "core.bare=true"]);
    }

    pub(crate) fn demand_session_compatible(&self, source: &OsStr) -> Result<bool> {
        if self.config.iter().any(|(key, value)| {
            let key = key.as_bytes();
            matches!(
                selected_remote_option(key, source),
                Some(b"uploadpack" | b"vcs" | b"serveroption")
            ) || key.starts_with(b"fsck.")
                || key == b"fetch.fsckobjects"
                || key == b"transfer.fsckobjects"
                || (key == b"protocol.version" && value.as_deref() != Some(OsStr::new("2")))
        }) {
            return Ok(false);
        }

        let Some(inherited) = std::env::var_os("GIT_EXEC_PATH") else {
            return Ok(true);
        };
        // Git exports its standard helper directory to external subcommands.
        // A custom directory can provide helpers outside the supported profile.
        let mut command = self.command(None);
        command
            .arg("--exec-path")
            .env_remove("GIT_EXEC_PATH")
            .stdin(Stdio::null());
        let result = run_builtin(command)?;
        Ok(result.status.success()
            && result.stdout.strip_suffix(b"\n") == Some(inherited.as_bytes()))
    }
    pub(crate) fn http2_compatible(&self, source: &OsStr, url: &OsStr) -> Result<bool> {
        if !self.transport_policy_is_default("https")
            || std::env::var_os("SSLKEYLOGFILE").is_some()
            || self.config.iter().any(|(key, _)| {
                let key = key.as_bytes();
                key.strip_prefix(b"http.")
                    .is_some_and(|option| !option.contains(&b'.'))
                    || matches!(
                        selected_remote_option(key, source),
                        Some(b"proxy" | b"proxyauthmethod")
                    )
            })
            || std::env::vars_os().any(|(key, _)| {
                let key = key.as_bytes();
                key.starts_with(b"GIT_HTTP_")
                    || key.starts_with(b"GIT_PROXY_SSL_")
                    || key.starts_with(b"GIT_SSL_") && key != b"GIT_SSL_CAINFO"
                    || matches!(
                        key,
                        b"GIT_TRACE_CURL"
                            | b"GIT_CURL_VERBOSE"
                            | b"GIT_SMART_HTTP"
                            | b"GIT_USER_AGENT"
                    )
            })
        {
            return Ok(false);
        }
        if !self
            .config
            .iter()
            .any(|(key, _)| key.as_bytes().starts_with(b"http."))
        {
            return Ok(true);
        }
        // Git knows its own URL matching rules for schemes, users, ports,
        // wildcard hosts and path prefixes. Consult it only for scoped keys.
        let mut command = native_git_command();
        command
            .current_dir(&self.cwd)
            .env_remove("GIT_CONFIG")
            .args(["config", "--includes", "--get-urlmatch", "http"])
            .arg(url)
            .stdin(Stdio::null());
        let output = run_builtin(command).context("check URL-specific Git HTTP configuration")?;
        match output.status.code() {
            Some(0) => Ok(false),
            Some(1) if output.stderr.is_empty() => Ok(true),
            _ => bail!(
                "check URL-specific Git HTTP configuration: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        }
    }
    pub(crate) fn transport_policy_is_default(&self, protocol: &str) -> bool {
        let key = format!("protocol.{protocol}.allow");
        !self
            .config
            .iter()
            .any(|(configured, _)| configured == "protocol.allow" || configured == key.as_str())
            && std::env::var_os("GIT_ALLOW_PROTOCOL").is_none()
            && std::env::var_os("GIT_PROTOCOL_FROM_USER").is_none()
    }

    pub(crate) fn has_multiple_group_members(&self, source: &OsStr) -> bool {
        self.config
            .iter()
            .filter(|(key, _)| key.as_bytes().strip_prefix(b"remotes.") == Some(source.as_bytes()))
            .filter_map(|(_, value)| value.as_deref())
            .flat_map(|value| {
                value
                    .as_bytes()
                    .split(|byte| matches!(byte, b' ' | b'\t' | b'\n'))
            })
            .filter(|member| !member.is_empty())
            .nth(1)
            .is_some()
    }

    pub fn output(&self, dir: Option<&Path>, args: &[&OsStr]) -> Result<Vec<u8>> {
        let reads_objects = args
            .first()
            .is_some_and(|operation| *operation == "rev-parse")
            && args.contains(&OsStr::new("--verify"));
        let mut command = self.command(dir);
        if reads_objects {
            command.arg("--no-lazy-fetch");
        }
        command.args(args).stdin(Stdio::null());
        let out = if local_query(args) {
            run_builtin(command).map_err(native_git_error)
        } else {
            run(command, true)
        }?;
        if !out.status.success() {
            if reads_objects {
                require_no_lazy_fetch(&out)?;
            }
            bail!(
                "Git {} failed: {}",
                args[0].to_string_lossy(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    }
    pub fn resolve(&self, revision: &OsStr) -> Result<Oid> {
        if index::selects_from_index(revision) {
            return index::resolve(self, revision).map(|(_, oid)| oid);
        }
        let out = self.output(
            None,
            &[
                OsStr::new("rev-parse"),
                OsStr::new("--verify"),
                OsStr::new("--end-of-options"),
                revision,
            ],
        )?;
        Oid::from_str(std::str::from_utf8(&out)?.trim())
            .context("resolve exact SHA-1 revision (SHA-256 repositories unsupported)")
    }
    pub fn repository_dir(&self) -> Result<&Path> {
        self.original_dir
            .as_deref()
            .context("native Git did not identify the selected repository directory")
    }
    fn verification_command(
        &self,
        dir: &Path,
        objects: Option<&Path>,
        common_dir: Option<&Path>,
    ) -> Command {
        let mut command = Command::new("git");
        command
            .current_dir(&self.cwd)
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .arg("--no-lazy-fetch");
        if self.original_dir.as_deref() == Some(dir) {
            return command;
        }
        // Keep Git's ordinary configuration stack and working directory.
        // The verifier for a fetched object reads only the private SHA-1 ODB.
        command
            .env(
                "GIT_OBJECT_DIRECTORY",
                objects.map_or_else(|| dir.join("objects"), Path::to_path_buf),
            )
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_QUARANTINE_PATH");
        if let Some(caller) = self
            .original_dir
            .as_deref()
            .filter(|_| common_dir.is_none())
        {
            if let Some(worktree) = &self.original_worktree {
                if !self.cwd.starts_with(worktree) {
                    // A detached core.worktree may not discover this Git dir.
                    command
                        .arg("--git-dir")
                        .arg(caller)
                        .arg("--work-tree")
                        .arg(worktree);
                } else if std::env::var_os("GIT_DIR").is_none()
                    && std::env::var_os("GIT_WORK_TREE").is_none()
                {
                    // Start at the worktree root to avoid discovering an
                    // incomplete nested repository through the private ODB.
                    command.current_dir(worktree);
                }
            }
        } else {
            // Keep the caller's Git directory identity for native conditional
            // includes. The common directory supplies a private SHA-1 config
            // and refs, and GIT_OBJECT_DIRECTORY supplies the fetched objects.
            let git_dir = common_dir.and(self.original_dir.as_deref()).unwrap_or(dir);
            command
                .arg("--git-dir")
                .arg(git_dir)
                .env_remove("GIT_COMMON_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .env_remove("GIT_SHALLOW_FILE")
                .env_remove("GIT_GRAFT_FILE");
            if let Some(common_dir) = common_dir {
                command.env("GIT_COMMON_DIR", common_dir);
                if let Some(worktree) = &self.original_worktree {
                    command.arg("--work-tree").arg(worktree);
                }
                // The SHA-1 view cannot resolve the caller's SHA-256 refs.
                // Preserve signature settings already evaluated on its branch.
                for (key, value) in &self.config {
                    if key.as_bytes().starts_with(b"gpg.") {
                        let mut setting = key.clone();
                        if let Some(value) = value {
                            setting.push("=");
                            setting.push(value);
                        }
                        command.arg("-c").arg(setting);
                    }
                }
            }
        }
        command
    }

    pub fn verify(&self, dir: &Path, oid: Oid, kind: git2::ObjectType) -> Result<()> {
        self.run_verifier(dir, oid, signature_verifier(kind)?, None, None)
    }

    pub(crate) fn verify_streamed_object(
        &self,
        run: &Path,
        object: &crate::pack_stream::VerifiedObject,
    ) -> Result<()> {
        let operation = signature_verifier(object.kind())?;
        // Verification finishes before copy fallback can create run-bound storage.
        use crate::service_storage::{io_out_of_space, Directory};
        match Directory::new("verify-") {
            Ok(directory) => match self.stage_verifier_object(directory.path(), object) {
                Ok(loose) => {
                    return self.verify_staged_object(directory.path(), loose, object, operation);
                }
                Err(error) if directory.can_fall_back_to(run) && io_out_of_space(&error) => {}
                Err(error) => return Err(error),
            },
            Err(error)
                if std::env::var_os("GIT_ON_SERVICE_TMPDIR").is_none()
                    && io_out_of_space(&error) => {}
            Err(error) => return Err(error),
        }
        let fallback = tempfile::Builder::new().prefix("verify-").tempdir_in(run)?;
        let loose = self.stage_verifier_object(fallback.path(), object)?;
        self.verify_staged_object(fallback.path(), loose, object, operation)
    }

    fn stage_verifier_object(
        &self,
        dir: &Path,
        object: &crate::pack_stream::VerifiedObject,
    ) -> Result<PathBuf> {
        if self.original_dir.is_none() {
            // Outside a repository, Git needs a bare context even for an OID.
            fs::create_dir(dir.join("refs"))?;
            fs::write(dir.join("HEAD"), "ref: refs/heads/master\n")?;
            fs::write(dir.join("config"), "[core]\n\tbare = true\n")?;
            let mut config = Vec::new();
            for (key, value) in &self.config {
                append_config_entry(
                    &mut config,
                    key.as_bytes(),
                    value.as_deref().map(OsStr::as_bytes),
                )?;
            }
            fs::write(dir.join("caller-config"), config)?;
        }
        let oid = object.oid().to_string();
        let fanout = dir.join("objects").join(&oid[..2]);
        fs::create_dir_all(&fanout)?;
        let loose = fanout.join(&oid[2..]);
        let mut encoder =
            flate2::write::ZlibEncoder::new(File::create_new(&loose)?, flate2::Compression::fast());
        encoder.write_all(
            crate::object::object_header(object.kind(), object.data().len()).as_bytes(),
        )?;
        crate::object::write_verified_buffer(object.data(), &mut encoder)?;
        encoder.finish()?;
        Ok(loose)
    }

    fn verify_staged_object(
        &self,
        dir: &Path,
        loose: PathBuf,
        object: &crate::pack_stream::VerifiedObject,
        operation: &str,
    ) -> Result<()> {
        let before = fs::symlink_metadata(&loose)?;
        self.run_verifier(dir, object.oid(), operation, None, Some(object))?;
        let file = File::options()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&loose)
            .context("reopen streamed verification object")?;
        let after = file.metadata()?;
        anyhow::ensure!(
            before.is_file()
                && (
                    before.dev(),
                    before.ino(),
                    before.ctime(),
                    before.ctime_nsec()
                ) == (after.dev(), after.ino(), after.ctime(), after.ctime_nsec()),
            "streamed verification object changed"
        );
        let mut decoder = flate2::read::ZlibDecoder::new(file);
        let header = crate::object::object_header(object.kind(), object.data().len());
        let mut actual = vec![0; header.len()];
        decoder.read_exact(&mut actual)?;
        anyhow::ensure!(
            actual == header.as_bytes(),
            "streamed verification object header changed"
        );
        crate::object::copy_verified(
            decoder,
            io::sink(),
            object.kind(),
            object.data().len(),
            object.oid(),
        )
    }

    fn run_verifier(
        &self,
        dir: &Path,
        oid: Oid,
        operation: &str,
        objects: Option<&Path>,
        streamed: Option<&crate::pack_stream::VerifiedObject>,
    ) -> Result<()> {
        let different_format = self
            .original_dir
            .as_deref()
            .is_some_and(|caller| caller != dir)
            && self.config.iter().any(|(key, value)| {
                key == "extensions.objectformat" && value.as_deref() != Some(OsStr::new("sha1"))
            });
        let config = if different_format {
            let source = self
                .caller_path(&["--path-format=absolute", "--git-path", "config"])
                .context("locate the caller's Git configuration")?;
            Some(prepare_verifier_config(&self.cwd, &source, dir)?)
        } else {
            None
        };
        let mut command =
            self.verification_command(dir, objects, config.as_ref().map(tempfile::TempDir::path));
        if self.original_dir.is_none() && streamed.is_some() {
            // The private bare Git dir must not activate a new conditional include.
            command
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", dir.join("caller-config"))
                .env_remove("GIT_CONFIG_COUNT")
                .env_remove("GIT_CONFIG_PARAMETERS");
        }
        let tag = streamed.filter(|object| object.kind() == ObjectType::Tag);
        command.arg(operation);
        if tag.is_some() {
            command.arg("--verbose");
        }
        command.arg(oid.to_string());
        // Native Git owns acceptance, including the caller's configured trust threshold.
        // Its default can accept unknown-trust OpenPGP; a git-on threshold here
        // would silently replace that policy.
        let output = run_captured(
            command,
            tag.is_some(),
            false,
            None,
            None,
            CaptureMode::Foreground,
        )?;
        if !output.status.success() {
            bail!("native Git {operation} failed for {oid}; execution forbidden");
        }
        if let Some(tag) = tag {
            let target = |data: &[u8]| crate::object::selected_child(ObjectType::Tag, data);
            // In verbose mode Git returns the payload used for signature verification.
            anyhow::ensure!(
                target(&output.stdout)? == target(tag.data())?,
                "native Git verified a different tag target"
            );
        }
        Ok(())
    }
    pub fn ssh_command(&self) -> Option<OsString> {
        std::env::var_os("GIT_SSH_COMMAND")
            .or_else(|| {
                self.config.iter().rev().find_map(|(key, value)| {
                    (key == "core.sshcommand").then(|| value.clone()).flatten()
                })
            })
            .or_else(|| {
                std::env::var_os("GIT_SSH").map(|path| {
                    // GIT_SSH is an executable path, not a shell command.
                    let mut quoted = vec![b'\''];
                    for byte in path.as_bytes() {
                        if *byte == b'\'' {
                            quoted.extend(b"'\\''");
                        } else {
                            quoted.push(*byte);
                        }
                    }
                    quoted.push(b'\'');
                    OsString::from_vec(quoted)
                })
            })
    }
    pub fn ssh_variant(&self) -> Option<OsString> {
        std::env::var_os("GIT_SSH_VARIANT").or_else(|| self.configured_ssh_variant())
    }

    fn configured_ssh_variant(&self) -> Option<OsString> {
        self.config
            .iter()
            .rev()
            .find(|(key, _)| key == "ssh.variant")
            .map(|(_, value)| value.clone().unwrap_or_default())
    }
}

fn selected_remote_option<'a>(key: &'a [u8], source: &OsStr) -> Option<&'a [u8]> {
    key.strip_prefix(b"remote.")?
        .strip_prefix(source.as_bytes())?
        .strip_prefix(b".")
}

fn read_configuration(cwd: &Path, options: &[&str]) -> Result<Vec<u8>> {
    let mut command = native_git_command();
    command
        .current_dir(cwd)
        // GIT_CONFIG overrides the config builtin alone. Fetch and
        // signature verification use the normal configuration stack.
        .env_remove("GIT_CONFIG")
        .args(["config", "--null", "--list"])
        .args(options)
        .stdin(Stdio::null());
    let output = run_builtin(command)
        .context("read effective Git configuration")
        .map_err(native_git_error)?;
    if !output.status.success() {
        bail!(
            "read effective Git configuration: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
}

fn is_config_include(key: &[u8]) -> bool {
    key.starts_with(b"include.") || key.starts_with(b"includeif.")
}

fn append_config_entry(config: &mut Vec<u8>, key: &[u8], value: Option<&[u8]>) -> Result<()> {
    let (section, name) = key.split_once_byte(b'.');
    let name = name.context("Git configuration key has no section")?;
    config.push(b'[');
    config.extend_from_slice(section);
    let name = if let Some(dot) = name.iter().rposition(|&byte| byte == b'.') {
        config.push(b' ');
        quote_config(config, &name[..dot], false);
        &name[dot + 1..]
    } else {
        name
    };
    config.extend_from_slice(b"]\n\t");
    config.extend_from_slice(name);
    if let Some(value) = value {
        config.extend_from_slice(b" = ");
        quote_config(config, value, true);
    }
    config.push(b'\n');
    Ok(())
}

fn prepare_verifier_config(cwd: &Path, source: &Path, git_dir: &Path) -> Result<tempfile::TempDir> {
    let source_dir = source
        .parent()
        .context("locate the caller's config directory")?;
    let output = read_configuration(cwd, &["--local", "--no-includes"])?;
    let mut config = b"[core]\n\trepositoryformatversion = 1\n".to_vec();
    for record in output
        .split(|&byte| byte == 0)
        .filter(|record| !record.is_empty())
    {
        let (key, value) = record.split_once_byte(b'\n');
        if key == b"core.repositoryformatversion"
            || (key.starts_with(b"extensions.") && key != b"extensions.worktreeconfig")
        {
            continue;
        }
        let include = is_config_include(key) && key.ends_with(b".path");
        let relocated_key = include
            .then(|| relocate_condition(key, source_dir))
            .flatten();
        let key = relocated_key.as_deref().unwrap_or(key);
        let relocated_path = value
            .filter(|value| {
                include && !value.starts_with(b"~") && !value.starts_with(b"%(prefix)/")
            })
            .map(|value| source_dir.join(OsStr::from_bytes(value)));
        let value = relocated_path
            .as_deref()
            .map(|path| path.as_os_str().as_bytes())
            .or(value);
        append_config_entry(&mut config, key, value)?;
    }
    // Supply a private native config scope for the SHA-1 verification object.
    let directory = tempfile::Builder::new()
        .prefix("verify-config-")
        .tempdir_in(git_dir)
        .context("create private native Git verification config directory")?;
    std::fs::create_dir(directory.path().join("refs"))?;
    std::fs::write(directory.path().join("config"), config)?;
    Ok(directory)
}

fn relocate_condition(key: &[u8], source_dir: &Path) -> Option<Vec<u8>> {
    let prefixes = [
        b"includeif.gitdir:./".as_slice(),
        b"includeif.gitdir/i:./".as_slice(),
    ];
    let (prefix, pattern) = prefixes
        .into_iter()
        .find_map(|prefix| key.strip_prefix(prefix).map(|pattern| (prefix, pattern)))?;
    let mut key = prefix[..prefix.len() - 2].to_vec();
    // "./" anchors the pattern at the original config's directory. Treat
    // that directory as literal when building an absolute Git glob.
    for &byte in source_dir.as_os_str().as_bytes() {
        if b"\\[*?".contains(&byte) {
            key.push(b'\\');
        }
        key.push(byte);
    }
    key.push(b'/');
    key.extend_from_slice(pattern);
    Some(key)
}

fn quote_config(out: &mut Vec<u8>, bytes: &[u8], escape_controls: bool) {
    out.push(b'"');
    for &byte in bytes {
        match byte {
            b'\\' | b'"' => out.extend([b'\\', byte]),
            b'\n' if escape_controls => out.extend(b"\\n"),
            b'\t' if escape_controls => out.extend(b"\\t"),
            8 if escape_controls => out.extend(b"\\b"),
            _ => out.push(byte),
        }
    }
    out.push(b'"');
}

pub(crate) fn signature_verifier(kind: ObjectType) -> Result<&'static str> {
    match kind {
        ObjectType::Commit => Ok("verify-commit"),
        ObjectType::Tag => Ok("verify-tag"),
        _ => bail!(
            "--verify requires a signed commit or annotated tag; raw trees and blobs have no signature"
        ),
    }
}

fn require_no_lazy_fetch(output: &std::process::Output) -> Result<()> {
    if output.status.code() == Some(129)
        && memchr::memmem::find(&output.stderr, b"--no-lazy-fetch").is_some()
    {
        bail!("local revision resolution requires native Git with --no-lazy-fetch (Git 2.45 or newer); upgrade Git or select a supported Git on PATH");
    }
    Ok(())
}

fn native_git_error(error: anyhow::Error) -> anyhow::Error {
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    }) {
        return error.context("native Git was not found on PATH; revision resolution requires native Git; install Git with your distribution tools or select an existing Git on PATH");
    }
    error
}

pub(crate) fn decode_url_bytes(mut encoded: &[u8]) -> Vec<u8> {
    let digit = |byte: u8| char::from(byte).to_digit(16).map(|value| value as u8);
    let mut path = Vec::new();
    while let Some((&byte, tail)) = encoded.split_first() {
        if let (b'%', [high, low, rest @ ..]) = (byte, tail) {
            if let (Some(high), Some(low)) = (digit(*high), digit(*low)) {
                let decoded = high * 16 + low;
                path.push(decoded);
                encoded = rest;
                continue;
            }
        }
        path.push(byte);
        encoded = tail;
    }
    path
}

fn local_query(args: &[&OsStr]) -> bool {
    // This command disables fsmonitor and automatic promisor fetches.
    matches!(args, [operation, ..] if *operation == "rev-parse")
        || matches!(args, [operation] if *operation == "--version")
        // The boundary prevents options from negating --get-url.
        || matches!(args, [operation, option, separator, _]
            if *operation == "ls-remote" && *option == "--get-url" && *separator == "--")
}
trait SplitByte {
    fn split_once_byte(&self, b: u8) -> (&[u8], Option<&[u8]>);
}
static CANCELLED: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static CANCELLATION_HANDLERS: std::sync::OnceLock<std::io::Result<()>> = std::sync::OnceLock::new();
static OPERATIONS_STOPPED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// End owned native operations after the command finishes without inventing a
/// termination signal. The runner process handles one command invocation.
pub fn stop_operations() {
    OPERATIONS_STOPPED.store(true, std::sync::atomic::Ordering::Relaxed);
}
pub fn operations_stopped() -> bool {
    OPERATIONS_STOPPED.load(std::sync::atomic::Ordering::Relaxed)
}
pub fn cancelled_signal() -> Option<i32> {
    let signal = CANCELLED.load(std::sync::atomic::Ordering::Relaxed);
    (signal != 0).then_some(signal)
}
/// Stop owned native operations when the foreground command is interrupted.
pub fn cancel_by_signal(signal: i32) {
    if matches!(
        signal,
        libc::SIGINT | libc::SIGTERM | libc::SIGHUP | libc::SIGQUIT
    ) {
        CANCELLED.store(signal, std::sync::atomic::Ordering::Relaxed);
    }
}
/// Register the cancellation latch before other subscribers can consume a
/// signal. Concurrent first callers wait for all four handlers, including a
/// registration failure, rather than observing partly initialized handlers.
pub fn initialize_cancellation() -> std::io::Result<()> {
    CANCELLATION_HANDLERS
        .get_or_init(|| {
            for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
                unsafe {
                    signal_hook::low_level::register(signal, move || {
                        CANCELLED.store(signal, std::sync::atomic::Ordering::Relaxed);
                    })?;
                }
            }
            Ok(())
        })
        .as_ref()
        .map(|_| ())
        .map_err(|error| std::io::Error::new(error.kind(), error.to_string()))
}
/// Wait for owned transport/verifier groups while draining their output and
/// servicing cancellation.
pub fn run(command: Command, capture: bool) -> Result<std::process::Output> {
    let managed = crate::terminal_ui::owns_terminal();
    let mut output = run_captured(
        command,
        capture || managed,
        capture || managed,
        None,
        None,
        CaptureMode::MayDetach,
    )?;
    if managed && !capture {
        use std::io::Write;
        let _drawing = crate::terminal_ui::terminal_handoff(true)?;
        std::io::stdout().write_all(&output.stdout)?;
        std::io::stderr().write_all(&output.stderr)?;
        output.stdout.clear();
        output.stderr.clear();
    }
    Ok(output)
}

/// Capture a Git builtin that does not invoke caller-configured helpers.
pub(crate) fn run_builtin(command: Command) -> Result<std::process::Output> {
    run_captured(command, true, true, None, None, CaptureMode::Channel)
}

pub(crate) fn capture_inherited_group(command: Command) -> Result<std::process::Output> {
    run_captured(command, true, true, None, None, CaptureMode::InheritedGroup)
}

fn trace_stream(value: &OsStr) -> Option<i32> {
    match value.as_bytes() {
        b"1" | b"2" | b"/dev/stderr" => Some(libc::STDERR_FILENO),
        b"/dev/stdout" => Some(libc::STDOUT_FILENO),
        bytes if bytes.eq_ignore_ascii_case(b"true") => Some(libc::STDERR_FILENO),
        _ => None,
    }
}

pub(crate) fn native_git_command() -> Command {
    let mut command = Command::new("git");
    // Git's traces and TLS key logging can open caller-supplied files.
    // Native commands keep their ordinary diagnostic streams.
    for (key, value) in std::env::vars_os() {
        if key.as_bytes().starts_with(b"GIT_TRACE") && trace_stream(&value).is_none() {
            command.env_remove(key);
        }
    }
    for key in ["GIT_TRACE2", "GIT_TRACE2_EVENT", "GIT_TRACE2_PERF"] {
        if std::env::var_os(key)
            .as_deref()
            .and_then(trace_stream)
            .is_none()
        {
            // Override destinations from system or user trace2 configuration.
            command.env(key, "0");
        }
    }
    command
        .env_remove("GIT_TRACE_PACKET")
        .env_remove("SSLKEYLOGFILE")
        .env("GIT_OPTIONAL_LOCKS", "0");
    command
}

#[doc(hidden)]
pub fn write_run_error(message: std::fmt::Arguments<'_>) {
    diagnostics::write(message, false);
}

pub(crate) fn write_live_error(message: std::fmt::Arguments<'_>) {
    // While the command owns the terminal, a background TOSTOP write would
    // suspend the runner before it can return the filesystem error. The
    // failure is retained for the final diagnostic after terminal handoff.
    if crate::terminal_ui::preparing() || unsafe { libc::isatty(libc::STDERR_FILENO) } == 0 {
        diagnostics::write(message, true);
    }
}

/// Stream native stdout to its consumer while the caller retains process,
/// cancellation and terminal supervision. The consumer must read to EOF or fail;
/// its errors stop and reap the owned operation before they are returned.
pub(crate) fn run_with_output<T: Send>(
    mut command: Command,
    input: Option<&mut dyn std::io::Read>,
    consume: impl FnOnce(&mut dyn std::io::Read) -> Result<T> + Send,
) -> Result<(std::process::Output, T)> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (reader, writer) = std::os::unix::net::UnixStream::pair()?;
    command.stdout(std::process::Stdio::from(std::os::fd::OwnedFd::from(
        writer,
    )));
    let failed = AtomicBool::new(false);
    let finished = AtomicBool::new(false);
    reader.set_read_timeout(Some(std::time::Duration::from_millis(10)))?;
    std::thread::scope(|scope| {
        let transfer = std::thread::Builder::new()
            .name("git-output".into())
            .spawn_scoped(scope, || {
                let mut output = NativeOutput {
                    reader,
                    finished: &finished,
                    start_drain: Some(|input: &mut std::os::unix::net::UnixStream| {
                        input.set_nonblocking(true)
                    }),
                };
                let result = consume(&mut output);
                failed.store(result.is_err(), Ordering::Release);
                result
            })?;
        let result = run_captured(
            command,
            false,
            true,
            Some(&failed),
            input,
            CaptureMode::MayDetach,
        );
        finished.store(true, Ordering::Release);
        let value = transfer
            .join()
            .map_err(|_| anyhow::anyhow!("native output consumer panicked"))??;
        Ok((result?, value))
    })
}

#[derive(Clone, Copy)]
pub(crate) enum BatchRead {
    Header,
    ReusableContents,
    Materialize,
}

impl BatchRead {
    pub(crate) fn has_contents(self) -> bool {
        !matches!(self, Self::Header)
    }
}

pub(crate) fn receive_batch_object<T>(
    oid: Oid,
    mode: BatchRead,
    mut input: &mut dyn BufRead,
    consume: impl FnOnce(usize, ObjectType, &mut dyn BufRead) -> Result<T>,
) -> Result<Result<T>> {
    use std::io::{self, Read};

    let mut header = String::new();
    (&mut input).take(128).read_line(&mut header)?;
    anyhow::ensure!(header.ends_with('\n'), "invalid native Git object header");
    let mut fields = header.split_ascii_whitespace();
    anyhow::ensure!(
        fields.next().and_then(|value| Oid::from_str(value).ok()) == Some(oid),
        "native Git returned a different object"
    );
    let kind = fields.next().context("missing native Git object type")?;
    if kind == "missing" && fields.next().is_none() {
        return Ok(Err(anyhow::anyhow!("missing Git object {oid}")));
    }
    let kind = ObjectType::from_str(kind).context("invalid native Git object type")?;
    let size: usize = fields.next().context("missing Git object size")?.parse()?;
    anyhow::ensure!(fields.next().is_none(), "invalid native Git object header");
    let mut data = (&mut input).take(if mode.has_contents() { size as u64 } else { 0 });
    let value = match consume(size, kind, &mut data) {
        // The outer error stops the batch immediately. Materialization has
        // already failed and will not send another request.
        Err(error)
            if matches!(mode, BatchRead::Materialize) || crate::object::is_cancelled(&error) =>
        {
            return Err(error);
        }
        value => value,
    };
    if mode.has_contents() {
        if value.is_err() {
            // Readers on a live mount may recover from a consumer error.
            io::copy(&mut data, &mut io::sink())?;
        }
        anyhow::ensure!(data.limit() == 0, "truncated native Git object");
        let mut newline = [0];
        input.read_exact(&mut newline)?;
        anyhow::ensure!(newline == *b"\n", "invalid native Git object delimiter");
    }
    Ok(value)
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn run_to_writer<W: std::io::Write + Send>(
    command: Command,
    mut output: W,
    input: Option<&mut dyn std::io::Read>,
) -> Result<std::process::Output> {
    run_with_output(command, input, |reader| {
        let mut bytes = [0; 64 * 1024];
        loop {
            let count = reader.read(&mut bytes)?;
            if count == 0 {
                return Ok(());
            }
            output.write_all(&bytes[..count])?;
        }
    })
    .map(|(result, ())| result)
}

struct NativeOutput<'a, R, F> {
    reader: R,
    finished: &'a std::sync::atomic::AtomicBool,
    start_drain: Option<F>,
}
impl<R: std::io::Read, F: FnOnce(&mut R) -> std::io::Result<()>> std::io::Read
    for NativeOutput<'_, R, F>
{
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        loop {
            // Preserve typed cancellation rather than reporting truncated Git
            // data after the supervisor closes an interrupted decoder's pipe.
            crate::object::check_transfer().map_err(std::io::Error::other)?;
            match self.reader.read(bytes) {
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if self.finished.load(std::sync::atomic::Ordering::Acquire) {
                        // A timeout can precede the final write/exit. Observe
                        // completion before freshly draining queued bytes.
                        match self.start_drain.take() {
                            Some(prepare) => prepare(&mut self.reader)?,
                            None => return Ok(0),
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }
}

pub(crate) fn controlling_terminal() -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()
}

fn run_captured(
    mut command: Command,
    capture_stdout: bool,
    capture_stderr: bool,
    failed_output: Option<&std::sync::atomic::AtomicBool>,
    input: Option<&mut dyn std::io::Read>,
    mode: CaptureMode,
) -> Result<std::process::Output> {
    use std::{
        os::unix::{io::AsRawFd, process::CommandExt},
        time::Duration,
    };
    initialize_cancellation()?;
    if let Some(signal) = cancelled_signal() {
        return Err(crate::object::OperationCancelled)
            .context(format!("Git operation cancelled by signal {signal}"));
    }
    if operations_stopped() {
        return Err(crate::object::OperationCancelled)
            .context("Git operation stopped after command completion");
    }
    let mut input = input
        .map(|source| NativeInput::new(&mut command, source))
        .transpose()?;
    let may_detach = matches!(mode, CaptureMode::MayDetach);
    let mut stdout = if capture_stdout {
        let (capture, writer) = NativeCapture::new(may_detach)?;
        command.stdout(writer);
        Some(capture)
    } else {
        None
    };
    let mut stderr = if capture_stderr {
        let (capture, writer) = NativeCapture::new(may_detach)?;
        command.stderr(writer);
        Some(capture)
    } else {
        None
    };
    let isolated = !matches!(mode, CaptureMode::InheritedGroup);
    let transport_terminal = isolated && !matches!(mode, CaptureMode::Foreground);
    let managed = transport_terminal && crate::terminal_ui::owns_terminal();
    if transport_terminal {
        crate::terminal_ui::configure(&mut command);
    }
    if managed && input.is_none() && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        command.stdin(Stdio::null());
    }
    let tty = (isolated && !managed).then(controlling_terminal).flatten();
    let tty_fd = tty.as_ref().map(AsRawFd::as_raw_fd);
    let parent_group = unsafe { libc::getpgrp() };
    let transfer = tty_fd.is_some_and(|fd| unsafe { libc::tcgetpgrp(fd) == parent_group });
    let _progress_lease = crate::terminal_ui::terminal_handoff(transfer)?;
    if isolated {
        unsafe {
            command.pre_exec(move || {
                if if managed {
                    libc::setsid()
                } else {
                    libc::setpgid(0, 0)
                } < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                if transfer {
                    set_foreground(tty_fd.unwrap(), libc::getpgrp())?;
                }
                Ok(())
            });
        }
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            if transfer {
                let _ = set_foreground(tty_fd.unwrap(), parent_group);
            }
            return Err(error).context("start native Git operation");
        }
    };
    drop(command);
    let _operation = isolated.then(|| crate::terminal_ui::NativeOperation::new(child.id()));
    let pid = child.id() as i32;
    let signal_target = if isolated { -pid } else { pid };
    let wakeup = ChildExitWakeup::new(child.id());
    let outcome = 'wait: loop {
        for capture in [&mut stdout, &mut stderr].into_iter().flatten() {
            if let Err(error) = capture.drain(64 * 1024) {
                break 'wait stop_request_operation(&mut child, signal_target)
                    .and(Err(error).context("read native Git output"));
            }
        }
        match crate::terminal_ui::try_wait(&mut child) {
            Ok(Some(status)) => break Ok(status),
            Err(error) => break Err(error.into()),
            Ok(None) => {}
        }
        if failed_output.is_some_and(|failed| failed.load(std::sync::atomic::Ordering::Acquire)) {
            break stop_request_operation(&mut child, signal_target)
                .and(Err(anyhow::anyhow!("native Git output consumer failed")));
        }
        if let Some(signal) = cancelled_signal() {
            unsafe {
                libc::kill(signal_target, libc::SIGKILL);
            }
            let _ = crate::terminal_ui::reap(&mut child);
            break Err(crate::object::OperationCancelled)
                .context(format!("Git operation cancelled by signal {signal}"));
        }
        if operations_stopped() {
            unsafe {
                libc::kill(signal_target, libc::SIGKILL);
            }
            let _ = crate::terminal_ui::reap(&mut child);
            break Err(crate::object::OperationCancelled)
                .context("Git operation stopped after command completion");
        }
        if let Some(stdin) = &mut input {
            match stdin.advance() {
                Ok(true) => input = None,
                Ok(false) => {}
                Err(error) => {
                    break stop_request_operation(&mut child, signal_target)
                        .and(Err(error).context("write native Git input"));
                }
            }
        }
        if let Some(fd) = tty_fd {
            if let Err(error) = relay_native_job_control(child.id(), fd, parent_group) {
                break stop_request_operation(&mut child, signal_target).and(Err(error));
            }
        }
        wakeup.wait(
            Duration::from_millis(10),
            input.as_ref().map(NativeInput::fd),
            [
                stdout.as_ref().and_then(NativeCapture::fd),
                stderr.as_ref().and_then(NativeCapture::fd),
            ],
        );
    };
    // A background resume must leave the terminal with the invoking shell.
    if let Some(fd) = tty_fd {
        if unsafe { libc::tcgetpgrp(fd) } == child.id() as libc::pid_t {
            let _ = set_foreground(fd, parent_group);
        }
    }
    let status = outcome?;
    use std::os::unix::process::ExitStatusExt;
    if let Some(signal) = status.signal() {
        // Terminal-generated signals go to the temporarily foreground Git
        // group, so the runner's own signal handler may never receive them.
        cancel_by_signal(signal);
    }
    Ok(std::process::Output {
        status,
        stdout: stdout
            .map(NativeCapture::finish)
            .transpose()?
            .unwrap_or_default(),
        stderr: stderr
            .map(NativeCapture::finish)
            .transpose()?
            .unwrap_or_default(),
    })
}

enum CaptureMode {
    Channel,
    InheritedGroup,
    MayDetach,
    Foreground,
}

enum NativeCapture {
    Channel {
        pipe: Option<std::os::unix::net::UnixStream>,
        bytes: Vec<u8>,
    },
    File(crate::service_storage::CaptureFile),
}

impl NativeCapture {
    fn new(may_detach: bool) -> Result<(Self, Stdio)> {
        if may_detach {
            // Native Git can leave a configured hook or a persistent SSH
            // proxy writing to its inherited descriptors after Git exits.
            // A capture channel would return EPIPE when its reader closes.
            let file = crate::service_storage::CaptureFile::new()?;
            let writer = file.file.try_clone()?.into();
            return Ok((Self::File(file), writer));
        }
        let (pipe, child) = std::os::unix::net::UnixStream::pair()?;
        pipe.set_nonblocking(true)?;
        Ok((
            Self::Channel {
                pipe: Some(pipe),
                bytes: Vec::new(),
            },
            Stdio::from(std::os::fd::OwnedFd::from(child)),
        ))
    }

    fn fd(&self) -> Option<i32> {
        match self {
            Self::Channel { pipe, .. } => pipe.as_ref().map(std::os::fd::AsRawFd::as_raw_fd),
            Self::File(_) => None,
        }
    }

    fn drain(&mut self, limit: u64) -> std::io::Result<()> {
        use std::io::{ErrorKind, Read};

        if let Self::Channel { pipe, bytes } = self {
            if let Some(reader) = pipe.as_ref() {
                match reader.take(limit).read_to_end(bytes) {
                    Ok(count) if (count as u64) < limit => *pipe = None,
                    Ok(_) => (),
                    Err(error) if error.kind() == ErrorKind::WouldBlock => (),
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }

    fn finish(mut self) -> std::io::Result<Vec<u8>> {
        if let Some(fd) = self.fd() {
            // Read bytes queued between the last drain and the child's exit.
            let mut available: libc::c_int = 0;
            if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut available) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            self.drain(available as u64)?;
        }
        match self {
            Self::Channel { bytes, .. } => Ok(bytes),
            Self::File(mut file) => {
                use std::io::{Read, Seek};
                let mut bytes = Vec::new();
                file.file.rewind()?;
                file.file.read_to_end(&mut bytes)?;
                Ok(bytes)
            }
        }
    }
}

struct NativeInput<'a> {
    source: &'a mut dyn std::io::Read,
    pipe: std::os::unix::net::UnixStream,
    bytes: [u8; 8192],
    pending: std::ops::Range<usize>,
}

impl<'a> NativeInput<'a> {
    fn new(command: &mut Command, source: &'a mut dyn std::io::Read) -> std::io::Result<Self> {
        use std::os::fd::OwnedFd;
        let (pipe, child) = std::os::unix::net::UnixStream::pair()?;
        pipe.set_nonblocking(true)?;
        command.stdin(Stdio::from(OwnedFd::from(child)));
        Ok(Self {
            source,
            pipe,
            bytes: [0; 8192],
            pending: 0..0,
        })
    }

    fn fd(&self) -> i32 {
        use std::os::fd::AsRawFd;
        self.pipe.as_raw_fd()
    }

    fn advance(&mut self) -> std::io::Result<bool> {
        use std::io::{ErrorKind, Write};
        // Bound work between cancellation checks, including for a decoder's
        // large segmented request stream. No input worker or full copy is needed.
        for _ in 0..8 {
            if self.pending.is_empty() {
                let count = match self.source.read(&mut self.bytes) {
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    value => value?,
                };
                if count == 0 {
                    return Ok(true);
                }
                self.pending = 0..count;
            }
            match self.pipe.write(&self.bytes[self.pending.clone()]) {
                Ok(0) => return Err(ErrorKind::WriteZero.into()),
                Ok(count) => self.pending.start += count,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(false),
                // A helper can reject a request before consuming stdin. Let
                // supervision collect its exit status and diagnostic output.
                Err(error) if error.kind() == ErrorKind::BrokenPipe => return Ok(true),
                Err(error) => return Err(error),
            }
        }
        Ok(false)
    }
}
/// Exit notification removes the artificial polling floor for short native
/// operations. Cancellation/TTY checks keep the same maximum polling interval.
/// Unsupported kernels or descriptor pressure retain the existing fallback.
/// Pollable child exit notification for native helpers.
pub struct ChildExitWakeup {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fd: Option<std::os::fd::OwnedFd>,
}
impl ChildExitWakeup {
    pub fn new(pid: u32) -> Self {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::FromRawFd;
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) } as i32;
            Self {
                fd: (fd >= 0).then(|| unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }),
            }
        }
        #[cfg(target_os = "macos")]
        {
            use std::os::fd::FromRawFd;

            // Darwin creates the descriptor close-on-exec. Poll wakes when
            // the notification arrives; the existing try_wait reaps the child.
            let queue = unsafe { libc::kqueue() };
            if queue < 0 {
                return Self { fd: None };
            }
            let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(queue) };
            let exit = libc::kevent {
                ident: pid as libc::uintptr_t,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD | libc::EV_ONESHOT,
                fflags: libc::NOTE_EXIT,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            let registered =
                unsafe { libc::kevent(queue, &exit, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
            Self {
                fd: (registered == 0).then_some(fd),
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = pid;
            Self {}
        }
    }
    pub fn wait(
        &self,
        duration: std::time::Duration,
        input: Option<i32>,
        output: [Option<i32>; 2],
    ) {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let exit = self.fd.as_ref().map_or(-1, std::os::fd::AsRawFd::as_raw_fd);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let exit = -1;
        let mut descriptors = [
            (exit, libc::POLLIN),
            (input.unwrap_or(-1), libc::POLLOUT),
            (output[0].unwrap_or(-1), libc::POLLIN),
            (output[1].unwrap_or(-1), libc::POLLIN),
        ]
        .map(|(fd, events)| libc::pollfd {
            fd,
            events,
            revents: 0,
        });
        let result = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as _,
                duration.as_millis() as i32,
            )
        };
        if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            std::thread::sleep(duration);
        }
    }
}
/// Native Git owns a separate group while authenticating/verifying. Observe
/// stops without consuming its exit status, then suspend the caller's job so
/// its shell can report the stop and perform ordinary `fg`/`bg` transitions.
fn relay_native_job_control(pid: u32, fd: i32, parent_group: libc::pid_t) -> Result<()> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result =
        unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WSTOPPED | libc::WNOHANG) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        // WSTOPPED alone can report an exited child as ECHILD. Child::try_wait
        // remains the sole owner of the exit status on the next iteration.
        if error.raw_os_error() == Some(libc::ECHILD)
            || error.kind() == std::io::ErrorKind::Interrupted
        {
            return Ok(());
        }
        return Err(error.into());
    }
    let group = pid as libc::pid_t;
    let foreground = unsafe { libc::tcgetpgrp(fd) };
    if foreground == parent_group {
        // `fg` can foreground an already-running background job without
        // sending SIGCONT. Pass ownership on during the normal polling loop.
        set_foreground(fd, group)?;
    }
    if crate::platform::siginfo_pid(&info) == 0 {
        return Ok(());
    }
    let stop = crate::platform::siginfo_status(&info);
    if foreground == parent_group && matches!(stop, libc::SIGTTIN | libc::SIGTTOU) {
        // A pending background-I/O stop raced the shell's `fg` transition.
        unsafe { libc::kill(-group, libc::SIGCONT) };
        return Ok(());
    }
    if unsafe { libc::tcgetpgrp(fd) } == group {
        set_foreground(fd, parent_group)?;
    }
    unsafe {
        libc::kill(-parent_group, libc::SIGSTOP);
    }
    // Only `fg` grants the caller's job the terminal. A `bg` resume leaves
    // it with the shell, so another terminal read can stop the job normally.
    if unsafe { libc::tcgetpgrp(fd) } == parent_group {
        set_foreground(fd, group)?;
    }
    unsafe { libc::kill(-group, libc::SIGCONT) };
    Ok(())
}
fn stop_request_operation(child: &mut std::process::Child, signal_target: i32) -> Result<()> {
    use std::time::{Duration, Instant};
    unsafe {
        libc::kill(signal_target, libc::SIGTERM);
    }
    // Let native Git roll back its locks. WNOWAIT preserves leader ownership
    // until every remaining group member has been killed, preventing PID reuse.
    let deadline = Instant::now() + Duration::from_millis(100);
    while matches!(crate::platform::child_exited(child.id()), Ok(false))
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    unsafe {
        libc::kill(signal_target, libc::SIGKILL);
    }
    crate::terminal_ui::reap(child).context("reap interrupted native Git operation")?;
    Ok(())
}
/// Transfer terminal ownership while blocking SIGTTOU in the calling thread.
pub fn set_foreground(fd: i32, group: libc::pid_t) -> std::io::Result<()> {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        let mut old = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTTOU);
        let error = libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
        if error != 0 {
            return Err(std::io::Error::from_raw_os_error(error));
        }
        let result = libc::tcsetpgrp(fd, group);
        let error = std::io::Error::last_os_error();
        libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
        if result != 0 {
            return Err(error);
        }
        Ok(())
    }
}
impl SplitByte for [u8] {
    fn split_once_byte(&self, b: u8) -> (&[u8], Option<&[u8]>) {
        match self.iter().position(|x| *x == b) {
            Some(n) => (&self[..n], Some(&self[n + 1..])),
            None => (self, None),
        }
    }
}

#[cfg(test)]
mod transport_policy_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn local_verification_command_keeps_worktree_relative_alternates() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let here = directory.path().join("worktree");
        git2::Repository::init(&here)?;
        let alternate = git2::Repository::init(directory.path().join("alternate"))?;
        let object = alternate.blob(b"only in the alternate")?;
        let invocation = here.join("inside");
        std::fs::create_dir(&invocation)?;
        let git = Git::capture(&invocation)?;
        let mut command = git.verification_command(git.repository_dir()?, None, None);
        command
            .env(
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                "../alternate/.git/objects",
            )
            .args(["cat-file", "-t", &object.to_string()]);
        let result = run_builtin(command)?;
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(result.stdout, b"blob\n");
        Ok(())
    }

    #[test]
    fn url_query_channels_require_the_exact_local_command() {
        let local = ["ls-remote", "--get-url", "--", "source"].map(OsStr::new);
        let remote = ["ls-remote", "--get-url", "--no-get-url", "source"].map(OsStr::new);
        assert!(local_query(&local));
        assert!(!local_query(&remote));
    }

    #[test]
    fn index_revision_resolution_does_not_run_fsmonitor_or_change_index() {
        use std::os::unix::fs::PermissionsExt;

        const FIXTURE_ENV: &str = "GIT_ON_TEST_FSMONITOR_FIXTURE";
        if std::env::var_os(FIXTURE_ENV).is_none() {
            let mut test = Command::new(std::env::current_exe().unwrap());
            for (key, _) in std::env::vars_os() {
                if key.as_bytes().starts_with(b"GIT_") {
                    test.env_remove(key);
                }
            }
            let result = test
                .env(FIXTURE_ENV, "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(["--exact", "git::transport_policy_tests::index_revision_resolution_does_not_run_fsmonitor_or_change_index"])
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let repo = git2::Repository::init(root).unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let signature = git2::Signature::now("Git-on tests", "test@example.invalid").unwrap();
        let commit = repo
            .commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
            .unwrap();
        assert!(Command::new("git")
            .current_dir(root)
            .args([
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{commit},view")
            ])
            .status()
            .unwrap()
            .success());

        let hook = root.join("fsmonitor");
        std::fs::write(
            &hook,
            br#"#!/bin/sh
printf called >helper-done
printf 'clock\000'
"#,
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
        repo.config()
            .unwrap()
            .set_str("core.fsmonitor", hook.to_str().unwrap())
            .unwrap();
        assert!(Command::new("git")
            .current_dir(root)
            .args([
                "-c",
                "core.fsmonitor=false",
                "update-index",
                "--split-index"
            ])
            .status()
            .unwrap()
            .success());
        let shared = std::fs::read_dir(repo.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .as_bytes()
                    .starts_with(b"sharedindex.")
            })
            .expect("split index base");
        let file = std::fs::File::options().write(true).open(&shared).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
            .unwrap();
        let modified = file.metadata().unwrap().modified().unwrap();
        let index = repo.path().join("index");
        let before = std::fs::read(&index).unwrap();
        let stale = "sharedindex.5555555555555555555555555555555555555555";
        std::os::unix::fs::symlink(stale, repo.path().join(stale)).unwrap();
        let (_, selected) = Git::capture_snapshot(root, OsStr::new(":view")).unwrap();
        assert_eq!(selected, commit);
        assert!(!root.join("helper-done").exists());
        assert_eq!(std::fs::read(&index).unwrap(), before);
        assert_eq!(
            std::fs::metadata(&shared).unwrap().modified().unwrap(),
            modified
        );
        // The base for an explicit GIT_INDEX_FILE may still be in the Git dir.
        let alternate = root.join("index-alternate");
        std::fs::copy(index, &alternate).unwrap();
        std::env::set_var("GIT_INDEX_FILE", &alternate);
        let (_, selected) = Git::capture_snapshot(root, OsStr::new(":0:view")).unwrap();
        assert_eq!(selected, commit);
        assert_eq!(
            std::fs::metadata(shared).unwrap().modified().unwrap(),
            modified
        );
    }

    #[test]
    fn native_capture_keeps_exit_status_after_child_closes_input() {
        use std::{
            io::{Read, Write},
            os::{fd::OwnedFd, unix::net::UnixStream},
            time::Duration,
        };

        let (sync, child_sync) = UnixStream::pair().unwrap();
        sync.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "read -r ready <&1; printf 'cannot read request\\n' >&2; exec 0<&-; printf x; exit 42",
            ])
            .stdout(Stdio::from(OwnedFd::from(child_sync)));
        struct RequestAfterInputCloses(Option<UnixStream>);
        impl Read for RequestAfterInputCloses {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                let Some(mut sync) = self.0.take() else {
                    return Ok(0);
                };
                sync.write_all(b"ready\n")?;
                sync.read_exact(&mut [0])?;
                bytes[0] = b'x';
                Ok(1)
            }
        }
        let output = run_captured(
            command,
            false,
            true,
            None,
            Some(&mut RequestAfterInputCloses(Some(sync))),
            CaptureMode::Channel,
        )
        .expect("preserve the outcome of a child that closes stdin");
        assert_eq!(output.status.code(), Some(42));
        assert_eq!(output.stderr, b"cannot read request\n");
    }

    #[test]
    fn native_capture_drains_both_streams_without_files() {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "dd if=/dev/zero bs=65536 count=4 2>/dev/null; cat >&2",
        ]);
        unsafe {
            command.pre_exec(|| {
                for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
                    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
                    if libc::fstat(fd, metadata.as_mut_ptr()) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if metadata.assume_init().st_mode & libc::S_IFMT == libc::S_IFREG {
                        return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
                    }
                }
                Ok(())
            });
        }
        let bytes = vec![b'x'; 1024 * 1024];
        let output = run_captured(
            command,
            true,
            true,
            None,
            Some(&mut bytes.as_slice()),
            CaptureMode::Channel,
        )
        .expect("capture Git output without regular files");
        assert!(output.status.success());
        assert_eq!(output.stdout, vec![0; 4 * 65536]);
        assert_eq!(output.stderr, bytes);
    }

    #[test]
    fn explicit_transport_policy_blocks_reusable_http2() {
        for key in ["protocol.allow", "protocol.https.allow"] {
            let git = Git {
                cwd: PathBuf::new(),
                config: vec![(key.into(), Some("never".into()))],
                original_dir: None,
                original_worktree: None,
            };
            assert!(
                !git.http2_compatible(
                    OsStr::new("origin"),
                    OsStr::new("https://example.invalid/repo.git")
                )
                .unwrap(),
                "{key}"
            );
        }
    }

    #[test]
    fn ssh_variant_uses_the_last_value() {
        let mut git = Git {
            cwd: PathBuf::new(),
            config: vec![
                ("ssh.variant".into(), None),
                ("ssh.variant".into(), Some("ssh".into())),
            ],
            original_dir: None,
            original_worktree: None,
        };
        assert_eq!(git.configured_ssh_variant(), Some("ssh".into()));
        git.config.push(("ssh.variant".into(), None));
        assert_eq!(git.configured_ssh_variant(), Some(OsString::new()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn detached_native_capture_is_memory_backed() -> Result<()> {
        use std::os::fd::AsRawFd;

        let capture = crate::service_storage::CaptureFile::new()?;
        let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
        assert_eq!(
            unsafe { libc::fstatfs(capture.file.as_raw_fd(), stats.as_mut_ptr()) },
            0
        );
        assert_eq!(unsafe { stats.assume_init() }.f_type, libc::TMPFS_MAGIC);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_input_and_output_progress_with_pipe_backpressure() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "dd if=/dev/zero bs=65536 count=4 2>/dev/null; cat"]);
        let bytes = vec![b'x'; 1024 * 1024];
        let mut input = bytes.as_slice();
        let mut output = Vec::new();
        let result = run_to_writer(command, &mut output, Some(&mut input)).unwrap();
        assert!(result.status.success());
        assert_eq!(&output[..262144], vec![0; 262144]);
        assert_eq!(&output[262144..], bytes);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_output_writer_error_stops_the_owned_operation() {
        struct Failed;
        impl std::io::Write for Failed {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("fixture output failure"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("not-stopped");
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "printf payload; sleep 3; printf late > \"$1\"",
                "fixture",
            ])
            .arg(&marker);
        let error = run_to_writer(command, Failed, None).unwrap_err();
        assert!(error.to_string().contains("fixture output failure"));
        assert!(
            !marker.exists(),
            "native operation continued after output failed"
        );
    }

    #[test]
    fn streamed_output_retains_the_native_exit_status() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf payload; exit 42"]);
        let (result, bytes) = run_with_output(command, None, |reader| {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes)?;
            Ok(bytes)
        })
        .unwrap();
        assert_eq!(bytes, b"payload");
        assert_eq!(result.status.code(), Some(42));
    }

    fn wait_for_pid(path: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(pid) = std::fs::read_to_string(path)
                .ok()
                .and_then(|pid| pid.trim().parse().ok())
            {
                return pid;
            }
            assert!(Instant::now() < deadline, "owned operation never started");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn interrupted_owned_group_stops_even_with_ignored_term_or_early_leader_exit() {
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        for leader_exits in [false, true] {
            let temporary = tempfile::tempdir().unwrap();
            let started = temporary.path().join("stubborn-pid");
            let script = if leader_exits {
                "sh -c 'trap \"\" TERM; echo $$ > \"$1\"; exec sleep 300' stubborn \"$1\" & wait"
            } else {
                "trap \"\" TERM; echo $$ > \"$1\"; exec sleep 300"
            };
            let mut child = Command::new("sh")
                .args(["-c", script, "owned-group-test"])
                .arg(&started)
                .process_group(0)
                .spawn()
                .unwrap();
            let pid = wait_for_pid(&started);
            let signal_target = -(child.id() as i32);
            stop_request_operation(&mut child, signal_target).unwrap();
            assert_eq!(
                child.wait().unwrap().signal(),
                Some(if leader_exits {
                    libc::SIGTERM
                } else {
                    libc::SIGKILL
                })
            );
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let state = std::fs::read_to_string(format!("/proc/{pid}/stat"));
                if state.is_err() || state.unwrap().split_whitespace().nth(2) == Some("Z") {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "owned descendant survived SIGKILL"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

#[cfg(test)]
mod output_race_tests {
    use super::*;
    use std::{
        io::{self, Read, Write},
        os::unix::net::UnixStream,
        sync::atomic::{AtomicBool, Ordering},
    };
    struct StaleTimeout<'a> {
        input: UnixStream,
        writer: UnixStream,
        finished: &'a AtomicBool,
        raced: bool,
    }
    impl Read for StaleTimeout<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if !self.raced {
                self.raced = true;
                let stale = self.input.read(output);
                assert!(
                    matches!(&stale, Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
                );
                // A native write and completed-child notification race a read
                // that has already returned a timeout to the transfer thread.
                self.writer
                    .write_all(b"verified output queued before exit")?;
                self.finished.store(true, Ordering::Release);
                return stale;
            }
            self.input.read(output)
        }
    }
    fn input(finished: &AtomicBool) -> StaleTimeout<'_> {
        let (input, writer) = UnixStream::pair().unwrap();
        input
            .set_read_timeout(Some(std::time::Duration::from_millis(1)))
            .unwrap();
        StaleTimeout {
            input,
            writer,
            finished,
            raced: false,
        }
    }
    #[test]
    fn finished_output_is_drained_after_a_stale_timeout() {
        let finished = AtomicBool::new(false);
        let mut input = input(&finished);
        let mut output = Vec::new();
        let mut reader = NativeOutput {
            reader: &mut input,
            finished: &finished,
            start_drain: Some(|input: &mut &mut StaleTimeout<'_>| {
                input.input.set_nonblocking(true)
            }),
        };
        io::copy(&mut reader, &mut output).unwrap();
        assert_eq!(output, b"verified output queued before exit");
    }
    #[test]
    fn final_output_drain_keeps_staging_failures_visible() {
        struct Failed;
        impl Write for Failed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("staging failed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let finished = AtomicBool::new(false);
        let mut input = input(&finished);
        let mut reader = NativeOutput {
            reader: &mut input,
            finished: &finished,
            start_drain: Some(|input: &mut &mut StaleTimeout<'_>| {
                input.input.set_nonblocking(true)
            }),
        };
        assert!(io::copy(&mut reader, &mut Failed).is_err());
    }
}
