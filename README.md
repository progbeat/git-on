# git-on

Run a command against one Git tree, with immutable source and temporary writes only where that tree's `.gitignore` permits them.

```sh
git on HEAD python script.py
git on HEAD:experiments uv run --locked experiment.py
git on 0123456789abcdef0123456789abcdef01234567 python -m pytest
```

`git-on` is a Git external command: installing the executable on `PATH` makes `git on` available. The first positional argument is always a Git tree-ish. Everything after it is a literal command and its arguments; no shell is introduced. To use shell syntax, explicitly run `sh -c '...'`.

## Installation and support

Supported: Linux, a working `/dev/fuse`, and `fusermount3` (typically the distribution's `fuse3` package). Build with current stable Rust and a C compiler:

```sh
cargo install --path . --locked
```

Containers must expose the FUSE device and allow mounts. The program never changes system permissions or enables these capabilities for you. macOS and Windows are not supported. Git object access uses libgit2; Git itself is needed for the integration tests.

## What the filesystem guarantees

The selected tree is resolved once to a full tree object ID. Commits, annotated tags pointing to commits, `HEAD:path`, and raw tree IDs work. The index, unstaged changes, untracked files, and later branch movement have no effect on the selected content. Commands start at the selected snapshot's root, even when invoked from a repository subdirectory.

- All paths present in the tree are immutable: contents, metadata, names, links, and directory entries cannot be replaced, removed, renamed, or shadowed.
- New files and directories may be created only when ignored by `.gitignore` files stored in the selected tree. Nested files, negation, anchored rules, and ignored-parent semantics apply. Tracked entries remain protected even when an ignore pattern matches them. Patterns containing 64 or more `*` bytes fail before execution rather than risking a matcher recursion-limit ambiguity.
- Global ignore files, `.git/info/exclude`, the live working tree, and runtime-created `.gitignore` files cannot grant write access.
- New ignored content uses a private per-run backing directory. It can be edited, deleted, or renamed only while respecting the same destination policy. Atomic temporary-file replacement works when both paths are allowed.
- Git executable bits and symlinks are preserved. Hard links cannot provide write access to tracked content. Special device nodes and extended attributes are not supported. Ordinary rename and no-replace rename are supported; exchange/whiteout rename is rejected.

For ordinary Python tools, commit patterns such as:

```gitignore
.venv/
__pycache__/
.pytest_cache/
```

Numba's normal `cache=True` cache can then live under `__pycache__` without a special environment override. A tool that writes a nonignored path fails rather than silently modifying source. A fresh run starts with a fresh writable layer; caches are reused during a run, not across runs. See `examples/` for locked uv script and project examples.

This is **not a security sandbox**. Commands inherit the caller's user identity and environment. Absolute paths, symlinks to outside the mount, network connections, and other accessible filesystems remain accessible. It protects the mounted snapshot, not the host. Do not run untrusted code on the strength of this filesystem policy.

Submodules and Git LFS pointer blobs fail before command execution. They are never silently treated as usable checked-out content. Git filters and hooks are not run. The mount contains source-tree entries, not a synthetic `.git` checkout. Git SHA-256 repositories are not currently supported by the bundled libgit2. The current implementation loads source blobs into memory before mounting, so memory usage scales with the selected tree's content; use a subtree to scope large repositories.

## Durable run evidence

Every attempted run creates a fresh private directory. Set `GIT_ON_RUN_DIR` to a path that does not yet exist, or use the default `$XDG_DATA_HOME/git-on/runs/<id>` (`$HOME/.local/share` when unset). Existing directories are never overwritten.

The directory contains:

- `manifest.json`: atomic updates recording a unique run ID, resolved tree ID, command argv, start/end timestamps, lifecycle phase, child exit/signal, runner errors, and log completeness
- `stdout.log`, `stderr.log`: separate raw byte logs, written live while also forwarding output to the caller
- `output/`: explicit persistent outputs, kept after the mount is removed

`argv` is readable UTF-8 with replacement for invalid bytes; `argv_hex` records exact Unix argv bytes. No environment dump is recorded. Arguments and output can contain secrets, so directories are mode 0700 and evidence files mode 0600. The caller remains responsible for retention and sharing.

The child receives these additional variables:

```text
GIT_ON_RUN_ID
GIT_ON_TREE_OID
GIT_ON_RUN_DIR
GIT_ON_OUTPUT_DIR
```

Other environment variables and stdin are inherited. There is no PTY: programs may buffer output according to their normal pipe behavior. Save results under `GIT_ON_OUTPUT_DIR` if they must survive the run. Ignored writable files inside the mount are temporary.

The manifest distinguishes a child's nonzero exit or signal from runner failure. A runner failure returns 125; usage errors return 2; otherwise the command's exit code is returned, or `128 + signal`. A child can itself exit 125, so consult the manifest to distinguish it. Logging/tee failure is a runner error, even when the command itself succeeds. Captured raw logs are retained when console forwarding fails.

## Foreground lifecycle and cleanup

The command gets its own process group. SIGINT, SIGTERM, SIGHUP, and SIGQUIT received by git-on are forwarded to that group. A controlling terminal is handed to the child and restored afterward; terminal stop/continue is relayed to the invoking foreground job. After the main child exits, remaining group members receive TERM followed by KILL. Pipe draining is bounded, so a detached process retaining a pipe cannot hang the runner indefinitely; incomplete capture is recorded as a runner error. This is a foreground runner, not a service daemonizer or orchestrator.

Git trees are pinned under `refs/git-on/<run-id>` while in use. A normal exit unmounts, releases the pin, and removes the temporary layer while preserving evidence and explicit outputs. If killed abruptly or the machine crashes, run:

```sh
git on --cleanup /path/to/run-directory
```

Cleanup refuses an active locked run, will only unmount a matching git-on filesystem, checks the pin's exact target before deleting it, and retains manifest/logs/output. A busy stale mount requires stopping processes using it first. Cleanup does not kill processes by stale PID. Once reviewed, delete old run directories yourself using ordinary filesystem tools. References left by interrupted runs remain until cleanup; they intentionally prevent garbage collection of that snapshot.

## Verification

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --locked
python3 tests/integration.py --binary target/debug/git-on CliTests
python3 tests/integration.py --binary target/debug/git-on --require-fuse --uv
```

The last command requires real FUSE and uv. GitHub Actions runs a separate required real-mount job, including locked uv/NumPy/Numba workloads, process lifecycle, concurrency, and adversarial filesystem operations. Missing mount capability is a failure, never an emulated pass. Unit tests do not substitute for kernel/FUSE integration evidence.
