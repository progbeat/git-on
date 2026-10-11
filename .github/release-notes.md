# 1.0.0
Bump: major

- Require `--` before every executed command: `git on REV -- COMMAND …`. Everything after the first separator is literal command argv, including additional `--`, flags, empty arguments, and non-UTF-8 bytes.
- Retain generated files with `git on REV --keep -- COMMAND …` or `git on REV -k -- COMMAND …`. Both flags also work before the revision, as in `git on --keep REV -- COMMAND …`; repeats combine harmlessly.
- Accept exactly one local revision before the separator. `REV:path` remains one unchanged operand. Reject a missing separator, extra operands, unknown runner options, and missing or empty revision/command names with a clear usage diagnostic and status 2 before Git resolution or runtime-state creation.
- Keep the existing first-token help, version, and cleanup modes. These do not execute a child command and need no separator.

**Migration from 0.1.x:** insert `--` before the command in every execution call. Change `git on REV COMMAND …` to `git on REV -- COMMAND …`, and `git on --keep REV COMMAND …` to `git on --keep REV -- COMMAND …`. The first separator always introduces the command; the old prefix revision-escape form is removed. Use a full object ID for a revision whose name starts with `-`.

This remains the same 1.0.0 major release under the project's CLI compatibility policy. Filesystem policy, retention/cleanup semantics, standard streams, and Linux/FUSE requirements are unchanged.
