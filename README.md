# git-on

[![CI](https://github.com/progbeat/git-on/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/progbeat/git-on/actions/workflows/ci.yml?query=branch%3Amaster)
[![Rust 1.89+](https://img.shields.io/badge/Rust-1.89%2B-93450a?logo=rust)](Cargo.toml)
[![Binaries: Linux x86_64](https://img.shields.io/badge/binaries-Linux_x86__64-0078d4?logo=linux&logoColor=white)](#installation-and-support)

Run commands against a Git revision without checking it out. Source files stay
immutable; temporary writes are allowed only where that revision's `.gitignore`
permits them.

```sh
git on HEAD python script.py
git on HEAD:experiments uv run --locked experiment.py
```

[Install](#installation-and-support) · [Quick start](#quick-start) ·
[Usage](docs/usage.md) · [Examples](examples/README.md) ·
[Benchmarks](tests/benchmark.md) · [Specification](docs/specification.md) ·
[Troubleshooting](docs/usage.md#troubleshooting)

- **One pinned tree.** Run committed code while leaving your working tree and
  index untouched. Branch movement during a run cannot change its source.
- **Load only what you use.** Files and directories are read on demand through
  FUSE, without unpacking or recursively scanning the repository at startup.
- **Disposable writes.** Each run gets a fresh writable layer for ignored paths.
  Remove it automatically, or retain it with `--keep`.
- **Ordinary commands.** Use the tools you already have. Arguments, standard
  streams, terminal behavior, and exit status follow the command through.

**This is not a security sandbox.** Commands retain your user identity, environment,
network access, and access to host files outside the mount. Run only code you trust.

## Installation and support

Prebuilt binaries target **Linux x86_64 with glibc 2.35+** (Ubuntu 22.04 baseline),
with the shared zlib (`libz.so.1`) and GCC (`libgcc_s.so.1`) runtimes.
Running git-on requires a usable `/dev/fuse` and `fusermount3`, usually provided by
your distribution's `fuse3` package. The installer needs `curl` and Python 3.8+;
Rust is not needed to run a prebuilt binary.

### Install or update

**Before the first release:** installation needs this installer on `master` and
a successful [release workflow](docs/releases.md). Until then, build from a
reviewed checkout. Anonymous installation also requires a public repository;
private-repository installation uses your existing GitHub CLI authentication.

Install the latest published stable release, or update an existing installation
with the same command:

```sh
curl -fsSL --proto '=https' --tlsv1.2 https://raw.githubusercontent.com/progbeat/git-on/master/install.sh | sh
```

The installer verifies release checksums and metadata before atomically replacing
`~/.local/bin/git-on`. Run it as your normal user, without `sudo`. It does not
modify shell startup files. Make the destination available in your current shell:

```sh
export PATH="$HOME/.local/bin:$PATH"
git on --version
```

See the [installation guide](docs/releases.md#installing-a-published-binary) for
an inspect-before-running alternative, pinned versions, custom destinations,
private releases, and checksum verification details.

### Build from source

In a checkout of the revision you want to install, use a current stable Rust
toolchain and a C compiler. The declared minimum Rust version is 1.89.

```sh
cargo install --path . --locked
```

Ensure Cargo's binary directory, normally `~/.cargo/bin`, is on `PATH`.
macOS and Windows are unsupported. Other Linux architectures have no prebuilt
release or CI support guarantee. Containers must expose `/dev/fuse` and permit
mounts; git-on does not change permissions or enable those capabilities for you.

## Quick start

After installing, run a command from an existing Git repository with a commit:

```sh
# List the committed tree, ignoring unstaged and untracked files.
git on HEAD ls

# Run a command against a previous commit instead.
git on HEAD~1 ls

# Ask for the command-line reference.
git on --help
```

The previous-commit example requires at least two commits. Commands start at the
selected tree's root, even when invoked from a repository subdirectory. A commit,
tag pointing to a commit, raw tree ID, or `HEAD:path/to/subdirectory` selects the
source. Use a full object ID when you want to repeat an exact snapshot.

### Let your tools write where they need to

Commit the appropriate ignore rules before selecting a tree. For example, a
Python project might need:

```gitignore
.venv/
__pycache__/
.pytest_cache/
```

These are examples, not built-in exceptions. Only `.gitignore` files in the
selected tree grant write access, and files already present in that tree remain
immutable even if they match an ignore rule. Missing rules produce ordinary
filesystem errors; editing your live `.gitignore` has no effect on a pinned run.

The repository includes [locked uv/NumPy/Numba examples](examples/README.md).
With uv available, run this from the git-on checkout after the examples are
committed:

```sh
git on HEAD:examples/uv-project uv run --locked main.py
```

### Keep results or use a pipeline

```sh
# Retain new ignored files after the command exits.
git on --keep HEAD:examples/uv-project uv run --locked main.py

# Use your shell to save stdout outside the mounted snapshot.
git on HEAD ls > committed-files.txt
```

`--keep` must precede the tree-ish. After unmounting, git-on prints the retained
temporary-directory path on stderr; generated files are under its `backing/`
subdirectory. Untouched source files are not copied there. Remove the directory
when you no longer need it. The next run still starts with an empty writable layer.

## Filesystem behavior

Tracked paths cannot be edited, removed, renamed, replaced, or shadowed. New paths
follow the selected tree's nested, negated, and anchored ignore rules, including
ignored-parent semantics. Global ignore files, `.git/info/exclude`, and
runtime-created ignore files cannot expand access.

Git executable bits and symlinks are preserved. Submodules and Git LFS content
are unsupported and fail when accessed; unrelated files remain usable. The mount
contains source files, not a synthetic `.git` checkout. Git filters and hooks are
not run. Git SHA-256 repositories are unsupported by the bundled libgit2.

See [usage and behavior](docs/usage.md) for the full write policy, streams,
environment, exit codes, signal handling, and recovery cleanup.

## Lazy access and performance

Startup resolves and pins one tree, then mounts its root. Lookup and reads load
requested metadata and content; directory listings do not recursively visit
children. Write checks load only the applicable pinned ignore files.

Retained caches have explicit budgets: 8 MiB / 128 blob entries, 4 MiB / 256 tree
entries, 4,096 blob headers, and 4 MiB / 128 compiled ignore-rule entries.
These are cache limits, not a total-memory guarantee. Git may decode a whole blob
or delta base for a partial read, and libgit2 and the kernel keep their own caches.
See the [cache and large-file behavior](docs/usage.md#lazy-access-and-performance).

The [million-file benchmark](tests/benchmark.md) verifies exact output and
object-demand counts, then reports timings and RSS with measurement limits.
Measurements are not a machine-independent speed promise. The
[specification](docs/specification.md#lazy-access-and-performance) defines the
lazy-access contract and required evidence.

## Verification and contributing

Start with the [behavioral specification](docs/specification.md) and
[contributor instructions](AGENTS.md). Local checks include:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --locked
python3 tests/integration.py --binary target/debug/git-on CliTests
```

[CI](https://github.com/progbeat/git-on/actions/workflows/ci.yml) additionally runs
real FUSE integration, locked uv workloads, a complete callback-inventory guard,
bounded randomized filesystem stress, and the million-file benchmark. Missing
mount capability is a failure in the required real-FUSE job, not an emulated
pass. The badge above reports `master`; use a pull request's checks for its exact
unmerged revision.

- [Test commands and requirements](tests/README.md)
- [FUSE callback coverage and evidence](docs/fuse-coverage.md)
- [Seeded stress tests and replay](tests/stress.md)
- [Review regressions](docs/review-regressions.md)
- [Versioning, release notes, and publication](docs/releases.md)

Every pull request includes the appropriate version bump and release notes.
Published history lives in [GitHub Releases](https://github.com/progbeat/git-on/releases).
If you are reading this in a release archive, the documentation links can be
browsed in [the GitHub repository](https://github.com/progbeat/git-on#readme).

For a bug report, include the git-on version, Linux distribution, Git tree ID,
minimal command and ignore rules, expected result, and actual error, with secrets
and private source removed.
