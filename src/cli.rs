use clap::{builder::OsStringValueParser, error::ErrorKind, Arg, ArgAction, Command};
use git_on::policy::WritePolicy;
use std::{ffi::OsString, os::unix::ffi::OsStrExt};

pub const RUN_USAGE: &str = "git on [REPOSITORY] REV [OPTIONS] -- COMMAND [ARGS...]";

#[derive(Debug, PartialEq)]
pub struct RunArgs<'a> {
    pub tree: OsString,
    pub repository: Option<OsString>,
    pub command: &'a [OsString],
    pub verify: bool,
    pub write_policy: WritePolicy,
}

#[derive(Debug, PartialEq)]
pub enum Invocation<'a> {
    Run(RunArgs<'a>),
    Cleanup(OsString),
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Help,
    Run,
    Cleanup,
}

// One argument definition serves generated help, run parsing and cleanup. Only
// first-token help/version/cleanup are control modes; the run parser omits them.
fn command(mode: Mode) -> Command {
    let mut command = Command::new("git-on")
        .bin_name("git on")
        .display_name("git-on")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Run argv with inherited standard I/O on a lazy Git snapshot")
        .disable_help_flag(true)
        .disable_version_flag(true)
        .args_override_self(true)
        .override_usage(RUN_USAGE)
        .after_help("Runner options may appear anywhere before the first --. The first -- is required before every command; all following argv is literal.\nOne Git operand is resolved by native git rev-parse; two operands are passed unchanged to native git fetch. Git owns repository and revision syntax.\nWrite policy: ignored (default), none (read-only), untracked (new paths), all (temporary copy-on-write changes). Source Git objects stay immutable.\nPrefer Linux FUSE or macFUSE. Without FUSE, only explicit --write-policy=all supports eager materialization. Temporary backing uses the system temporary directory (TMPDIR).\nGIT_ON_TREE_OID identifies the pinned source tree. Cleanup: git on --cleanup DIRECTORY\nUse git on -h for help through Git. Exit: child status; runner failure 125; usage 2.");
    if mode != Mode::Cleanup {
        command = command
            .arg(
                Arg::new("verify")
                    .long("verify")
                    .action(ArgAction::SetTrue)
                    .help("Verify the exact commit/tag signature with native Git"),
            )
            .arg(
                Arg::new("write-policy")
                    .long("write-policy")
                    .require_equals(true)
                    .value_name("POLICY")
                    .value_parser(["none", "ignored", "untracked", "all"])
                    .default_value("ignored")
                    .help("Temporary writes: none, ignored, untracked, or all"),
            )
            .arg(
                Arg::new("operands")
                    .value_name("REV")
                    .num_args(1..=2)
                    .action(ArgAction::Append)
                    .required(true)
                    .value_parser(OsStringValueParser::new())
                    .help("Local REV, or REPOSITORY REV passed unchanged to native Git"),
            );
    }
    if mode != Mode::Run {
        command = command.arg(
            Arg::new("cleanup")
                .long("cleanup")
                .value_name("DIRECTORY")
                .num_args(1)
                .exclusive(true)
                .value_parser(OsStringValueParser::new())
                .help("Remove a validated stale temporary run"),
        );
    }
    if mode == Mode::Help {
        command = command
            .arg(
                Arg::new("help")
                    .long("help")
                    .short('h')
                    .action(ArgAction::Help)
                    .help("Print help"),
            )
            .arg(
                Arg::new("version")
                    .long("version")
                    .action(ArgAction::Version)
                    .help("Print version"),
            );
    }
    command
}

pub fn parse(args: &[OsString]) -> Result<Invocation<'_>, clap::Error> {
    if args
        .first()
        .is_some_and(|arg| arg == "--help" || arg == "-h" || arg == "--version")
    {
        // Preserve first-token precedence and ignored trailing arguments.
        let result =
            command(Mode::Help).try_get_matches_from([OsString::from("git-on"), args[0].clone()]);
        return Err(result.expect_err("help/version actions return their display result"));
    }
    if args.first().is_some_and(|arg| arg == "--cleanup") {
        let mut command = command(Mode::Cleanup).override_usage("git on --cleanup DIRECTORY");
        if args.len() != 2 {
            return Err(command.error(
                ErrorKind::WrongNumberOfValues,
                "--cleanup requires exactly one temporary-directory argument",
            ));
        }
        // An attached value preserves literal '--', option-looking, empty and
        // non-UTF-8 cleanup paths without giving them control-mode semantics.
        let mut value = OsString::from("--cleanup=");
        value.push(&args[1]);
        let matches = command.try_get_matches_from([OsString::from("git-on"), value])?;
        return Ok(Invocation::Cleanup(
            matches.get_one::<OsString>("cleanup").unwrap().clone(),
        ));
    }
    parse_run_args(args).map(Invocation::Run)
}

pub fn parse_run_args(args: &[OsString]) -> Result<RunArgs<'_>, clap::Error> {
    let mut command = command(Mode::Run);
    let boundary = args.iter().position(|arg| arg == "--").ok_or_else(|| {
        command.error(
            ErrorKind::MissingRequiredArgument,
            "missing -- separator before the command",
        )
    })?;
    let prefix = &args[..boundary];
    let mut options = Vec::new();
    let mut operands = Vec::new();
    for arg in prefix {
        if arg == "--keep" || arg == "-k" || arg.as_bytes().starts_with(b"--keep=") {
            return Err(command.error(ErrorKind::UnknownArgument,
                "--keep and -k were removed in 1.3.0; write logs and artifacts to an explicit external path"));
        }
        if command.get_arguments().any(|definition| {
            definition.get_long().is_some_and(|long| {
                let Some(value) = arg.as_bytes().strip_prefix(b"--") else {
                    return false;
                };
                value == long.as_bytes()
                    || (value.split(|byte| *byte == b'=').next() == Some(long.as_bytes())
                        && value.contains(&b'='))
            }) || definition
                .get_short()
                .is_some_and(|short| arg.as_bytes() == [b'-', short as u8])
        }) {
            options.push(arg.clone());
        } else {
            operands.push(arg.clone());
        }
    }
    // Parse only our own options. A synthetic boundary protects every other
    // token from Clap interpretation; Git receives its original bytes, even
    // for an empty or option-looking repository/revision.
    let matches = command.clone().try_get_matches_from(
        std::iter::once(OsString::from("git-on"))
            .chain(options)
            .chain(std::iter::once(OsString::from("--")))
            .chain(operands),
    )?;
    let operands: Vec<_> = matches
        .get_many::<OsString>("operands")
        .unwrap()
        .cloned()
        .collect();
    // One local Git operand or a repository/refspec pair selects one snapshot.
    let (repository, tree) = match operands.as_slice() {
        [tree] => (None, tree.clone()),
        [repository, tree] => (Some(repository.clone()), tree.clone()),
        _ => {
            return Err(command.error(
                ErrorKind::TooManyValues,
                "expected REV or REPOSITORY REV before --",
            ))
        }
    };
    let command_tail = &args[boundary + 1..];
    if command_tail.first().is_none_or(|arg| arg.is_empty()) {
        return Err(command.error(
            ErrorKind::MissingRequiredArgument,
            "expected a non-empty command after --",
        ));
    }
    Ok(RunArgs {
        tree,
        repository,
        command: command_tail,
        verify: matches.get_flag("verify"),
        write_policy: matches
            .get_one::<String>("write-policy")
            .unwrap()
            .parse()
            .unwrap(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    fn arguments(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn assert_run(args: &[&str], tree: &str, command: &[&str]) {
        let args = arguments(args);
        let parsed = parse_run_args(&args).unwrap();
        assert_eq!(parsed.tree, tree);
        assert_eq!(parsed.command, command);
    }

    #[test]
    fn removed_keep_options_are_rejected_only_before_separator() {
        for flag in ["--keep", "-k", "--keep=1"] {
            for args in [[flag, "HEAD", "--", "cmd"], ["HEAD", flag, "--", "cmd"]] {
                let error = parse_run_args(&arguments(&args)).unwrap_err();
                assert_eq!(error.exit_code(), 2);
                assert!(error.to_string().contains("were removed"));
            }
        }
        assert_run(
            &["HEAD", "--", "cmd", "--keep", "-k"],
            "HEAD",
            &["cmd", "--keep", "-k"],
        );
    }

    #[test]
    fn mandatory_separator_rejects_old_direct_command_syntax() {
        for args in [
            vec!["HEAD", "cmd"],
            vec!["--keep", "HEAD", "cmd"],
            vec!["-k", "HEAD", "cmd"],
        ] {
            assert!(parse_run_args(&arguments(&args)).is_err(), "{args:?}");
        }
    }

    #[test]
    fn first_separator_forwards_the_entire_command_tail() {
        let command = ["--keep", "-k", "--unknown", "--", "", "a b"];
        let mut args = vec!["HEAD", "--"];
        args.extend(command);
        assert_run(&args, "HEAD", &command);
        assert_run(&["HEAD", "--", "--", "arg"], "HEAD", &["--", "arg"]);
        for name in ["--help", "-h", "--version", "--cleanup"] {
            assert_run(&["HEAD", "--", name], "HEAD", &[name]);
        }
    }

    #[test]
    fn subtree_revision_is_one_unchanged_operand() {
        let tree = "HEAD:examples/uv-project";
        assert_run(&[tree, "--", "cmd"], tree, &["cmd"]);
    }
    #[test]
    fn write_policy_accepts_exact_values_and_keeps_command_tail_literal() {
        for policy in ["none", "ignored", "untracked", "all"] {
            let option = format!("--write-policy={policy}");
            for prefix in [vec!["HEAD", &option], vec![&option, "HEAD"]] {
                let mut args = arguments(&prefix);
                args.extend(arguments(&["--", "cmd", "--write-policy=none"]));
                let parsed = parse_run_args(&args).unwrap();
                assert_eq!(parsed.write_policy, policy.parse().unwrap());
                assert_eq!(parsed.command, arguments(&["cmd", "--write-policy=none"]));
            }
        }
        let args = arguments(&[
            "origin",
            "HEAD",
            "--write-policy=none",
            "--write-policy=untracked",
            "--write-policy=all",
            "--",
            "cmd",
        ]);
        assert_eq!(
            parse_run_args(&args).unwrap().write_policy,
            WritePolicy::All
        );
        let args = arguments(&["HEAD", "--", "cmd"]);
        assert_eq!(
            parse_run_args(&args).unwrap().write_policy,
            WritePolicy::Ignored
        );
        for option in [
            "--write-policy=",
            "--write-policy=other",
            "--write-policy=ALL",
            "--write-policy",
        ] {
            assert_eq!(
                parse_run_args(&arguments(&["HEAD", option, "--", "cmd"]))
                    .unwrap_err()
                    .exit_code(),
                2
            );
        }
    }
    #[test]
    fn remote_operands_verify_and_literal_command_tail() {
        for source in [
            "origin",
            "https://example.invalid/repo.git",
            "ssh://git@example.invalid/repo",
        ] {
            let args = arguments(&[
                "--verify",
                source,
                "main:deep",
                "--verify",
                "--",
                "cmd",
                "--verify",
                "-k",
                "--",
            ]);
            let parsed = parse_run_args(&args).unwrap();
            assert_eq!(parsed.repository.unwrap(), source);
            assert_eq!(parsed.tree, "main:deep");
            assert!(parsed.verify);
            assert_eq!(parsed.command, arguments(&["cmd", "--verify", "-k", "--"]));
        }
    }

    #[test]
    fn malformed_prefix_and_missing_command_are_usage_errors() {
        for args in [
            vec![],
            vec!["-k"],
            vec!["--"],
            vec!["--", "HEAD", "cmd"],
            vec!["--keep", "--", "cmd"],
            vec!["repo", "HEAD", "extra", "--", "cmd"],
            vec!["HEAD", "--"],
            vec!["HEAD", "-k", "--", ""],
        ] {
            let error = parse_run_args(&arguments(&args)).unwrap_err();
            assert_eq!(error.exit_code(), 2, "{args:?}");
            assert!(error.use_stderr(), "{args:?}");
        }
    }

    #[test]
    fn git_operands_are_not_validated_as_runner_options() {
        for flag in ["--unknown", "--help", "--version", "--cleanup", "-kk", "-"] {
            for args in [
                vec![flag, "HEAD", "--", "cmd"],
                vec!["HEAD", flag, "--", "cmd"],
            ] {
                let raw = arguments(&args);
                let parsed = parse_run_args(&raw).unwrap();
                assert_eq!(parsed.repository.as_ref(), Some(&raw[0]), "{args:?}");
                assert_eq!(parsed.tree, raw[1], "{args:?}");
            }
        }
        for operands in [vec![""], vec!["", "HEAD"], vec!["repo", ""]] {
            let mut args = arguments(&operands);
            args.extend(arguments(&["--", "cmd"]));
            let parsed = parse_run_args(&args).unwrap();
            assert_eq!(parsed.tree, operands[operands.len() - 1]);
        }
        assert!(parse_run_args(&arguments(&["HEAD", "--keep=1", "--", "cmd"])).is_err());
    }

    #[test]
    fn non_utf8_revision_command_and_arguments_are_preserved() {
        let tree = OsString::from_vec(b"tree-\xff:subtree".to_vec());
        let raw = OsString::from_vec(b"-\xfe".to_vec());
        let args = vec![tree.clone(), "--".into(), raw.clone(), raw, "".into()];
        let parsed = parse_run_args(&args).unwrap();
        assert_eq!(parsed.tree, tree);
        assert_eq!(parsed.command, &args[2..]);
        let args = vec![
            "HEAD".into(),
            OsString::from_vec(b"-\xff".to_vec()),
            "--".into(),
            "cmd".into(),
        ];
        let parsed = parse_run_args(&args).unwrap();
        assert_eq!(
            parsed.repository.as_deref(),
            Some(std::ffi::OsStr::new("HEAD"))
        );
        assert_eq!(parsed.tree, args[1]);
    }
    #[test]
    fn interspersed_flags_do_not_relax_total_operand_limit() {
        for args in [
            vec!["repo", "--verify", "HEAD", "extra", "--", "cmd"],
            vec!["repo", "HEAD", "--verify", "extra", "--", "cmd"],
            vec!["repo", "--verify", "HEAD", "--verify", "extra", "--", "cmd"],
            vec![
                "a", "--verify", "b", "--verify", "c", "--verify", "d", "--", "cmd",
            ],
        ] {
            let error = parse_run_args(&arguments(&args)).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::TooManyValues, "{args:?}");
            assert_eq!(error.exit_code(), 2);
        }
    }

    #[test]
    fn control_modes_keep_first_token_precedence_and_version_bytes() {
        for (flag, kind) in [
            ("--help", ErrorKind::DisplayHelp),
            ("-h", ErrorKind::DisplayHelp),
            ("--version", ErrorKind::DisplayVersion),
        ] {
            for trailing in [
                vec![],
                vec!["--nope"],
                vec!["HEAD", "--", "--help"],
                vec!["--version", "--help"],
            ] {
                let mut args = vec![flag];
                args.extend(trailing);
                let error = parse(&arguments(&args)).unwrap_err();
                assert_eq!(error.kind(), kind);
                assert_eq!(error.exit_code(), 0);
                assert!(!error.use_stderr());
                if flag == "--version" {
                    assert_eq!(
                        error.to_string(),
                        format!("git-on {}\n", env!("CARGO_PKG_VERSION"))
                    );
                }
            }
        }
    }

    #[test]
    fn cleanup_paths_are_literal_os_strings() {
        for path in [
            OsString::from("--"),
            "--help".into(),
            "--version".into(),
            "".into(),
            OsString::from_vec(b"path-\xff".to_vec()),
        ] {
            let args = vec!["--cleanup".into(), path.clone()];
            assert_eq!(parse(&args).unwrap(), Invocation::Cleanup(path));
        }
        for args in [vec!["--cleanup"], vec!["--cleanup", "one", "two"]] {
            assert_eq!(parse(&arguments(&args)).unwrap_err().exit_code(), 2);
        }
    }
}
