# 1.0.0
Bump: major

- Retain generated files with `git on REV --keep -- COMMAND …` or the short form `git on REV -k -- COMMAND …`. Both flags also work before the revision; repeated retention flags combine harmlessly.
- Keep ordinary `git on REV COMMAND …` calls compatible. Arguments after the command name or explicit separator remain literal, including `--keep`, `-k`, further `--` tokens, empty arguments, and non-UTF-8 bytes.
- Reject unknown runner options and missing or empty revision/command names with usage status 2 before resolving Git objects or creating runtime state.
- Reserve the post-revision `--` separator. Calls from 0.1.x that execute a program named `--`, or a dash-prefixed program followed by `--` before any non-option argument, need an explicit command boundary: use `git on REV -- --keep -- args` instead of `git on REV --keep -- args`. Escape dash-prefixed revision names with `git on -- REV COMMAND …`.

This is a major bump under the project's CLI compatibility policy, including before 1.0. Filesystem policy, retention/cleanup semantics, standard streams, and Linux/FUSE requirements are unchanged.
