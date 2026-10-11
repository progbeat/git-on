use std::{ffi::OsString, fmt, os::unix::ffi::OsStrExt};

pub const RUN_USAGE: &str = "git on [--keep|-k] <tree-ish> [--keep|-k] -- <command> [args...]";

#[derive(Debug, PartialEq)]
pub struct RunArgs<'a> {
    pub tree: &'a OsString,
    pub command: &'a [OsString],
    pub keep: bool,
}

#[derive(Debug, PartialEq)]
pub enum UsageError {
    MissingSeparator,
    RevisionCount,
    EmptyRevision,
    UnknownOption,
    MissingCommand,
}

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MissingSeparator => "missing -- separator before the command",
            Self::RevisionCount => "expected exactly one local revision before --",
            Self::EmptyRevision => "revision must not be empty",
            Self::UnknownOption => "unknown runner option before -- (supported: --keep, -k)",
            Self::MissingCommand => "expected a non-empty command after --",
        })
    }
}

pub fn parse_run_args(args: &[OsString]) -> Result<RunArgs<'_>, UsageError> {
    let boundary = args
        .iter()
        .position(|arg| arg == "--")
        .ok_or(UsageError::MissingSeparator)?;
    let mut operands = Vec::new();
    let mut keep = false;
    for arg in &args[..boundary] {
        if arg == "--keep" || arg == "-k" {
            keep = true;
        } else if arg.is_empty() {
            return Err(UsageError::EmptyRevision);
        } else if arg.as_bytes().starts_with(b"-") {
            return Err(UsageError::UnknownOption);
        } else {
            operands.push(arg);
        }
    }
    // One positional operand currently selects a local revision. Command argv
    // always starts after the delimiter, independently of operand cardinality.
    let [tree] = operands.as_slice() else {
        return Err(UsageError::RevisionCount);
    };
    let command = &args[boundary + 1..];
    if command.first().is_none_or(|arg| arg.is_empty()) {
        return Err(UsageError::MissingCommand);
    }
    Ok(RunArgs {
        tree,
        command,
        keep,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    fn arguments(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn assert_run(args: &[&str], tree: &str, command: &[&str], keep: bool) {
        let args = arguments(args);
        let parsed = parse_run_args(&args).unwrap();
        assert_eq!(parsed.tree, tree);
        assert_eq!(parsed.command, command);
        assert_eq!(parsed.keep, keep);
    }

    #[test]
    fn keep_accepts_both_spellings_on_either_side_of_revision() {
        for flag in ["--keep", "-k"] {
            assert_run(&[flag, "HEAD", "--", "cmd"], "HEAD", &["cmd"], true);
            assert_run(&["HEAD", flag, "--", "cmd"], "HEAD", &["cmd"], true);
        }
        assert_run(
            &["-k", "--keep", "HEAD", "--keep", "-k", "--", "cmd"],
            "HEAD",
            &["cmd"],
            true,
        );
    }

    #[test]
    fn mandatory_separator_rejects_old_direct_command_syntax() {
        for args in [
            vec!["HEAD", "cmd"],
            vec!["--keep", "HEAD", "cmd"],
            vec!["-k", "HEAD", "cmd"],
            vec!["HEAD", "cmd", "--", "arg"],
            vec!["HEAD", "--keep", "cmd", "--", "arg"],
        ] {
            assert!(parse_run_args(&arguments(&args)).is_err(), "{args:?}");
        }
    }

    #[test]
    fn first_separator_forwards_the_entire_command_tail() {
        let command = ["--keep", "-k", "--unknown", "--", "", "a b"];
        for options in [vec![], vec!["-k"], vec!["--keep", "-k"]] {
            let mut args = vec!["HEAD"];
            args.extend(&options);
            args.push("--");
            args.extend(command);
            assert_run(&args, "HEAD", &command, !options.is_empty());
        }
        assert_run(&["HEAD", "--", "--", "arg"], "HEAD", &["--", "arg"], false);
        for name in ["--help", "-h", "--version", "--cleanup"] {
            assert_run(&["HEAD", "--", name], "HEAD", &[name], false);
        }
    }

    #[test]
    fn subtree_revision_is_one_unchanged_operand() {
        let tree = "HEAD:examples/uv-project";
        assert_run(&[tree, "--", "cmd"], tree, &["cmd"], false);
        assert_run(&[tree, "-k", "--", "cmd"], tree, &["cmd"], true);
    }

    #[test]
    fn malformed_prefix_and_missing_command_are_usage_errors() {
        for (args, error) in [
            (vec![], UsageError::MissingSeparator),
            (vec!["-k"], UsageError::MissingSeparator),
            (vec!["--"], UsageError::RevisionCount),
            (vec!["--", "HEAD", "cmd"], UsageError::RevisionCount),
            (vec!["--keep", "--", "cmd"], UsageError::RevisionCount),
            (vec!["repo", "HEAD", "--", "cmd"], UsageError::RevisionCount),
            (vec!["HEAD", "--"], UsageError::MissingCommand),
            (vec!["HEAD", "-k", "--", ""], UsageError::MissingCommand),
            (vec!["", "--", "cmd"], UsageError::EmptyRevision),
        ] {
            assert_eq!(parse_run_args(&arguments(&args)), Err(error), "{args:?}");
        }
    }

    #[test]
    fn unknown_options_are_rejected_on_either_side_of_revision() {
        for flag in [
            "--unknown",
            "--help",
            "--version",
            "--cleanup",
            "-kk",
            "--keep=1",
            "-",
        ] {
            for args in [
                vec![flag, "HEAD", "--", "cmd"],
                vec!["HEAD", flag, "--", "cmd"],
            ] {
                assert_eq!(
                    parse_run_args(&arguments(&args)),
                    Err(UsageError::UnknownOption),
                    "{args:?}"
                );
            }
        }
    }

    #[test]
    fn non_utf8_revision_command_and_arguments_are_preserved() {
        let tree = OsString::from_vec(b"tree-\xff:subtree".to_vec());
        let raw = OsString::from_vec(b"-\xfe".to_vec());
        let args = vec![
            tree.clone(),
            "-k".into(),
            "--".into(),
            raw.clone(),
            raw,
            "".into(),
        ];
        let parsed = parse_run_args(&args).unwrap();
        assert_eq!(parsed.tree, &tree);
        assert_eq!(parsed.command, &args[3..]);
        assert!(parsed.keep);
        let args = vec![
            "HEAD".into(),
            OsString::from_vec(b"-\xff".to_vec()),
            "--".into(),
            "cmd".into(),
        ];
        assert_eq!(parse_run_args(&args), Err(UsageError::UnknownOption));
    }
}
