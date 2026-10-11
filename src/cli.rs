use std::{ffi::OsString, os::unix::ffi::OsStrExt};

pub const RUN_USAGE: &str = "git on [--keep|-k] [--] <tree-ish> <command> [args...]\n       git on [--keep|-k] [--] <tree-ish> [--keep|-k] -- <command> [args...]";

#[derive(Debug, PartialEq)]
pub struct RunArgs<'a> {
    pub tree: &'a OsString,
    pub command: &'a [OsString],
    pub keep: bool,
}

fn is_keep(arg: &OsString) -> bool {
    arg == "--keep" || arg == "-k"
}

pub fn parse_run_args(args: &[OsString]) -> Option<RunArgs<'_>> {
    let mut offset = 0;
    let mut keep = false;
    while let Some(arg) = args.get(offset) {
        if arg == "--" {
            offset += 1;
            break;
        }
        if is_keep(arg) {
            keep = true;
            offset += 1;
        } else if arg.as_bytes().starts_with(b"-") {
            return None;
        } else {
            break;
        }
    }
    let tree = args.get(offset).filter(|arg| !arg.is_empty())?;
    let mut command = &args[offset + 1..];

    // Only the initial option-shaped block can reserve a command boundary.
    // Never search past a command name or inspect anything after the first `--`.
    if let Some(boundary) = command
        .iter()
        .take_while(|arg| arg.as_bytes().starts_with(b"-"))
        .position(|arg| arg == "--")
    {
        if !command[..boundary].iter().all(is_keep) {
            return None;
        }
        keep |= boundary > 0;
        command = &command[boundary + 1..];
    }
    if command.first().is_none_or(|arg| arg.is_empty()) {
        return None;
    }
    Some(RunArgs {
        tree,
        command,
        keep,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    fn assert_run(args: &[&str], tree: &str, command: &[&str], keep: bool) {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let parsed = parse_run_args(&args).unwrap();
        assert_eq!(parsed.tree, tree);
        assert_eq!(parsed.command, command);
        assert_eq!(parsed.keep, keep);
    }

    #[test]
    fn keep_accepts_both_spellings_and_positions() {
        for flag in ["--keep", "-k"] {
            assert_run(&[flag, "HEAD", "cmd"], "HEAD", &["cmd"], true);
            assert_run(&["HEAD", flag, "--", "cmd"], "HEAD", &["cmd"], true);
            assert_run(&[flag, "--", "HEAD", "cmd"], "HEAD", &["cmd"], true);
        }
        assert_run(
            &["-k", "--keep", "--", "HEAD", "--keep", "-k", "--", "cmd"],
            "HEAD",
            &["cmd"],
            true,
        );
    }

    #[test]
    fn legacy_tail_is_literal_without_an_initial_boundary() {
        for command in [
            vec!["cmd", "-k", "--keep", "--unknown", "--", ""],
            vec!["-k", "cmd", "--"],
            vec!["--keep"],
            vec!["--unknown", "cmd", "--"],
            vec!["--help"],
            vec!["--version"],
            vec!["--cleanup", "dir"],
        ] {
            let mut args = vec!["HEAD"];
            args.extend(&command);
            assert_run(&args, "HEAD", &command, false);
        }
    }

    #[test]
    fn first_boundary_forwards_the_entire_command_tail() {
        let command = ["--keep", "-k", "--unknown", "--", "", "a b"];
        for options in [vec![], vec!["-k"], vec!["--keep", "-k"]] {
            let mut args = vec!["HEAD"];
            args.extend(&options);
            args.push("--");
            args.extend(command);
            assert_run(&args, "HEAD", &command, !options.is_empty());
        }
        assert_run(&["HEAD", "--", "--", "arg"], "HEAD", &["--", "arg"], false);
    }

    #[test]
    fn prefix_boundary_escapes_revision_but_suffix_boundary_is_independent() {
        for tree in ["--keep", "-k", "--", "-odd"] {
            assert_run(&["--", tree, "cmd"], tree, &["cmd"], false);
            assert_run(&["--", tree, "-k", "--", "cmd"], tree, &["cmd"], true);
        }
    }

    #[test]
    fn malformed_run_arguments_are_usage_errors() {
        for args in [
            vec![],
            vec!["--keep"],
            vec!["-k"],
            vec!["--"],
            vec!["HEAD"],
            vec!["--keep", "HEAD"],
            vec!["HEAD", "--"],
            vec!["HEAD", "-k", "--"],
            vec!["HEAD", "--keep", "--"],
            vec!["", "cmd"],
            vec!["HEAD", ""],
            vec!["HEAD", "--", ""],
            vec!["--unknown", "HEAD", "cmd"],
            vec!["-kk", "HEAD", "cmd"],
            vec!["--keep=1", "HEAD", "cmd"],
            vec!["HEAD", "--unknown", "--", "cmd"],
            vec!["HEAD", "-k", "--unknown", "--", "cmd"],
            vec!["HEAD", "--keep=1", "--", "cmd"],
            vec!["HEAD", "-kk", "--", "cmd"],
        ] {
            let args: Vec<OsString> = args.iter().map(OsString::from).collect();
            assert_eq!(parse_run_args(&args), None, "{args:?}");
        }
    }

    #[test]
    fn non_utf8_revision_command_and_arguments_are_preserved() {
        let tree = OsString::from_vec(b"tree-\xff".to_vec());
        let raw = OsString::from_vec(b"-\xfe".to_vec());
        let args = vec![tree.clone(), "-k".into(), "--".into(), raw.clone(), raw];
        let parsed = parse_run_args(&args).unwrap();
        assert_eq!(parsed.tree, &tree);
        assert_eq!(parsed.command, &args[3..]);
        assert!(parsed.keep);
        let args = vec![tree, OsString::from_vec(b"-\xff".to_vec()), "cmd".into()];
        assert_eq!(parse_run_args(&args).unwrap().command, &args[1..]);
        let args = vec!["HEAD".into(), args[1].clone(), "--".into(), "cmd".into()];
        assert_eq!(parse_run_args(&args), None);
    }
}
