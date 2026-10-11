# Usage and behavior

[Back to the README](../README.md) · [Behavioral specification](specification.md)

[Syntax](#command-syntax) · [Write policy](#filesystem-behavior) · [Streams and
results](#standard-streams-and-temporary-writes) ·
[Cleanup](#foreground-lifecycle-and-cleanup) ·
[Performance](#lazy-access-and-performance) · [Troubleshooting](#troubleshooting)

## Command syntax

```text
git on [--keep] [--] <tree-ish> <command> [args...]
git on --cleanup <temporary-directory>
git on --help
git on --version
```

`git-on` is a Git external command: an executable named `git-on` on `PATH` makes `git
on` available. It can also be invoked directly as `git-on`.

Runner flags must precede the tree-ish. Everything after it is a literal command and its
arguments, including an argument spelled `--keep`. An optional `--` ends runner-option
parsing before the tree-ish. No shell is introduced; to use shell syntax, explicitly run
`sh -c '...'`.

## Filesystem behavior

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
git on --keep HEAD python script.py
```

`--keep` must appear before the tree-ish. After unmounting, git-on reports the retained
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
  `--keep` before the tree-ish, redirect output in the invoking shell, or have the
  command save chosen files to an explicit host path.

For errors after an interrupted run, see [recovery
cleanup](#foreground-lifecycle-and-cleanup).
