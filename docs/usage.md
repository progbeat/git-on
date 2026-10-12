# Usage and behavior

[Back to the README](../README.md) · [Behavioral specification](specification.md)

[Syntax](#command-syntax) · [Write policy](#filesystem-behavior) · [Streams and
results](#standard-streams-and-temporary-writes) ·
[Cleanup](#foreground-lifecycle-and-cleanup) ·
[Performance](#lazy-access-and-performance) · [Troubleshooting](#troubleshooting)

## Command syntax

```text
git on [REPOSITORY] REV [OPTIONS] -- COMMAND [ARGS...]
git on --cleanup <temporary-directory>
git on -h
git on --version
```

`git-on` is a Git external command: an executable named `git-on` on `PATH` makes
`git on` available. It can also be invoked directly as `git-on`.

Every executed command requires `--`. Before the first separator, supply exactly
one local revision, or a repository followed by a revision, then any
`--keep`/`-k`/`--verify` options after REV. Both keep spellings
have the same effect; repeating them is harmless. `HEAD:path/to/subtree` remains
one revision operand. Everything after that first separator is literal command
argv: git-on does not parse the command's options or any subsequent `--`.

```sh
git on HEAD -- ls
git on HEAD:examples/uv-project -k -- uv run --locked main.py
git on HEAD:examples/uv-project --keep -- uv run --locked main.py
```

| Invocation | Interpretation |
| --- | --- |
| `git on HEAD --keep -- cmd -k --keep -- arg` | Keep files; pass `-k --keep -- arg` to `cmd` |
| `git on HEAD -k -- cmd --keep` | Keep files; pass `--keep` to `cmd` |
| `git on HEAD -- cmd -k -- arg` | Default cleanup; pass `-k -- arg` to `cmd` |
| `git on HEAD cmd` | Usage error: missing command separator |
| `git on origin HEAD -- cmd` | Run the advertised HEAD from configured remote origin |
| `git on HEAD --unknown -- cmd` | Usage error: unknown runner option |

Missing `--`, missing or empty revision/command names, extra positional operands,
and unknown runner options return 2 with a diagnostic before resolving Git
objects or creating runtime state. Bundles such as `-kk` and values such as
`--keep=true` are not runner flags. Tokens starting with `-` before the separator
are treated as options; to select a revision with such a name, use its full
object ID. The first `--` is always the command boundary and cannot escape a
revision. Empty later arguments and non-UTF-8 argv bytes remain intact. No shell
is introduced; to use shell syntax, explicitly run `sh -c '...'`.

`git on -h`, `git on --version`, and `git on --cleanup` do not execute a child
command and do not require that separator. Direct `git-on --help` also prints
help. The same spellings after the separator are ordinary command argv.

**Migrating from 0.1.x:** insert `--` before the command in every execution call:
`git on REV command args` becomes `git on REV -- command args`.
To retain generated files, use `git on REV --keep -- command args`.
For a rare executable named `--keep`, use `git on REV -- --keep -- args`: the
second `--` belongs to that executable.

## Filesystem behavior

For HTTPS/SSH URLs, named remotes, cache bounds and native signature verification,
see [the remote guide](remote.md).

The selected tree is resolved once to a full tree object ID. Commits, annotated tags
pointing to commits, `HEAD:path`, and raw tree IDs work. The index, unstaged changes,
untracked files, and later branch movement have no effect on the selected content.
Commands start at the selected snapshot's root, even when invoked from a repository
subdirectory.

- All paths present in the tree are immutable: contents, metadata, names, links, and
  directory entries cannot be replaced, removed, renamed, or shadowed.
- New files and directories may be created only when ignored by `.gitignore` files
  stored in the selected tree. Nested files, negation, anchored rules, and
  ignored-parent semantics apply. Tracked entries remain protected even when an ignore
  pattern matches them.
- Global ignore files, `.git/info/exclude`, the live working tree, and runtime-created
  `.gitignore` files cannot grant write access.
- New ignored content uses a private per-run backing directory. It can be edited,
  deleted, or renamed only while respecting the same destination policy. Atomic
  temporary-file replacement works when both paths are allowed.
- Git executable bits and symlinks are preserved. Hard links cannot provide write access
  to tracked content. Special device nodes and extended attributes are not supported.
  Ordinary rename and no-replace rename are supported; exchange/whiteout rename is
  rejected.
- A denied filesystem operation returns an ordinary filesystem error to its caller.
  git-on does not terminate the application merely because an operation was denied; the
  application handles that error normally.
- The writable layer is temporary and private to one run. Normal completion removes it
  unless `--keep` was requested; a new run starts empty. Abrupt termination can leave
  backing files until checked cleanup runs, as described below.

The policy is language-independent. There are no special write exceptions for Python,
uv, Numba, or any other tool. See [examples](../examples/README.md) for optional
application configurations.

This is **not a security sandbox**. Commands inherit the caller's user identity and
environment. Absolute paths, symlinks to outside the mount, network connections, and
other accessible filesystems remain accessible. It protects the mounted snapshot, not
the host. Do not run untrusted code on the strength of this filesystem policy.

Submodules fail when their metadata is accessed; Git LFS pointer blobs fail when their
contents are read. Unused unsupported entries do not prevent starting a command or
reading unrelated files. Directory listings can show submodule names. They are never
silently treated as usable checked-out content. Git filters and hooks are not run. The
mount contains source-tree entries, not a synthetic `.git` checkout. Git SHA-256
repositories are not currently supported by the bundled libgit2.

## Standard streams and temporary writes

The command inherits stdin, stdout, and stderr directly. Terminal detection, pipes,
redirection, binary output, and buffering behave as they do for an ordinary child
process. git-on does not tee or capture these streams and creates no logs, run manifest,
or output directory. Use ordinary shell redirection or a separate experiment runner when
you need durable records.

The only environment variable git-on adds or overrides is `GIT_ON_TREE_OID`, the full
selected tree ID. The rest of the caller's environment is inherited. No `GIT_ON_RUN_ID`,
`GIT_ON_RUN_DIR`, or `GIT_ON_OUTPUT_DIR` is generated or consumed. If callers already
set those names, they remain ordinary inherited variables.

New ignored files use a private directory in the system temporary location, respecting
`TMPDIR`. Storage uses ordinary filesystem-backed files, not a private memory
filesystem, and there is no git-on disk quota. If the system's temporary location is
itself a memory filesystem, choose a disk-backed location with `TMPDIR`. Filesystem
capacity and operating-system resource limits still apply. Temporary directories are
private to the current user.

By default, git-on unmounts and removes the writable layer after the command finishes.
To retain newly written ignored files:

```sh
git on HEAD --keep -- python script.py
```

The equivalent short form is `git on HEAD -k -- python script.py`. After unmounting,
git-on reports the retained
temporary-directory path on stderr. Its `backing/` subdirectory contains the writable
layer with its original paths, not a checkout of the selected tree; untouched tracked
source files are not copied into it. The mount is removed. A small retention marker
remains so recovery cleanup cannot mistake intentionally kept files for disposable stale
state. A later invocation still starts with an empty writable layer. Remove the retained
directory yourself when finished with it. Alternatively, write or copy chosen results to
an explicit host path from the command.

Usage errors return 2; a runner failure returns 125; otherwise the command's exit code
is returned. If the command terminates by signal, git-on re-raises that signal after
cleanup; shells normally display this as `128 + signal`. A child may itself return 125,
so that code alone cannot distinguish a runner failure. Runner diagnostics go to stderr.

## Foreground lifecycle and cleanup

The command gets its own process group. SIGINT, SIGTERM, SIGHUP, and SIGQUIT received by
git-on are forwarded to that group. A controlling terminal is handed to the child and
restored afterward; terminal stop/continue is relayed to the invoking foreground job.
After the main child exits, remaining group members receive TERM followed by KILL.
git-on does not drain or wait for captured-output pipes because it creates none. A
detached process outside the managed group can still retain inherited descriptors;
git-on is a foreground runner, not a service daemonizer or host process sandbox.

Git trees are pinned under `refs/git-on/<id>` while in use. Normal completion unmounts,
releases the pin, and removes temporary state. With `--keep`, only the writable backing
directory is retained. A busy mount or abrupt termination can leave a temporary
directory, mount, and pin; minimal private recovery metadata allows checked cleanup
without keeping execution logs or a run manifest:

```sh
git on --cleanup /path/to/stale-temporary-directory
```

Cleanup refuses an active locked run or a deliberately kept backing directory, will only
unmount a matching git-on filesystem, and checks the pin's exact target before deleting
it. A busy stale mount requires stopping processes using it first. Cleanup does not kill
processes by stale PID. A surviving pin intentionally prevents garbage collection until
cleanup releases it.

SIGKILL is uncatchable and does not forward a signal to the command. The command
may keep running in its own process group. git-on does not enable FUSE
`auto_unmount`; killing the runner after mounting leaves a disconnected mount
whose fresh directory accesses fail with `ENOTCONN`. Temporary files, pins and a
recorded remote SSH master may remain. Stop the command and any other mount users
first, then use the cleanup command above. The same recovery can be necessary if
the runner dies during mount setup before launching the command. Cleanup does
not stop surviving commands or infer process ownership from old PIDs.

## Lazy access and performance

The [behavioral and performance specification](specification.md) defines the contract.
Startup resolves and pins the selected tree and mounts its root; it does not recursively
enumerate the source. Lookup loads ancestor directory metadata and the requested blob
header, directory listing loads only that directory, and read loads the requested
content. Write checks load only applicable pinned ignore files.

Decoded content retention is bounded: 8 MiB / 128 blob entries, 4 MiB / 256 tree
entries, 4,096 blob headers, and 4 MiB / 128 compiled ignore-rule entries. Idle open
file descriptors do not pin evicted small payloads. Kernel inode references and active
directory listings retain their necessary metadata until released. libgit2 also has its
own internal object cache with a default 256 MiB soft target; its default blob caching
is disabled.

Git encoding can require decoding an entire requested blob or its delta bases. Large
blobs use an immediately unlinked spool in the private backing directory, shared by
simultaneous readers and removed after their final close; subsequent ranges in that open
file do not repeatedly decode the object. A first read still needs the memory to decode
that object. These limits are retained-cache budgets, not a bound on total RSS, kernel
cache, active metadata, or temporary large-object decoding.

The [benchmark](../tests/benchmark.md) checks exact output and demand counts on a
million distinct files using a separate, opt-in instrumented test build. Its counters
exclude initial tree-ish resolution and do not measure physical I/O or hidden delta-base
reads. The harness captures all evidence externally; normal builds have no metrics
output. Latency and RSS are measured, not promised as a machine-independent SLA.

## Troubleshooting

- **`git: 'on' is not a git command`:** ensure the directory containing `git-on` is on
  `PATH`. Check `command -v git-on` and `git-on --version`.
- **FUSE device or mount errors:** check that `/dev/fuse` is available and `fusermount3`
  is installed. Containers need host-granted mount capabilities; installing the binary
  alone cannot provide them.
- **A tool cannot write:** check the `.gitignore` committed in the selected tree,
  including ignored-parent and negation rules. A matching tracked file is still
  immutable. Adding a pattern in the live checkout does not change that tree.
- **A script or recent edit is missing:** git-on reads the selected revision. Staged,
  unstaged, and untracked changes do not appear until committed and selected.
- **Generated results disappear:** the writable layer is deleted by default. Use
  `--keep`/`-k` using the syntax above, redirect output in the invoking shell, or have the
  command save chosen files to an explicit host path.

For errors after an interrupted run, see [recovery
cleanup](#foreground-lifecycle-and-cleanup).
