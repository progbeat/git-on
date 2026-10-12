# git-on

[![CI (master)](https://github.com/progbeat/git-on/actions/workflows/ci.yml/badge.svg?branch=master&event=push)](https://github.com/progbeat/git-on/actions/workflows/ci.yml?query=branch%3Amaster+event%3Apush)
[![Release](https://img.shields.io/github/v/release/progbeat/git-on)](https://github.com/progbeat/git-on/releases)
[![Rust 1.89+](https://img.shields.io/badge/Rust-1.89%2B-blue?logo=rust&logoColor=white)](Cargo.toml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Run commands against a Git revision without checking it out. Source files stay
immutable; temporary writes follow that revision's `.gitignore` rules.

```sh
git on HEAD -- python script.py
git on HEAD:experiments -- uv run --locked experiment.py
```

[Installation](#installation) · [Quick start](#quick-start) ·
[Documentation](#documentation)

- **Pinned source.** Run committed code without changing your working tree or
  index. The selected tree stays fixed for the entire command.
- **Lazy access.** Read files and directories on demand through FUSE, without
  unpacking or scanning the whole repository at startup.
- **Disposable writes.** Each run gets a fresh writable layer for ignored paths.
  Clean it up automatically, or retain it with `--keep`.

## Installation

Prebuilt releases support **Linux x86_64 with glibc 2.35+** (Ubuntu 22.04 baseline),
plus the shared zlib and GCC runtimes (`libz.so.1`, `libgcc_s.so.1`). Running git-on
requires a usable `/dev/fuse` and `fusermount3`, usually from your distribution's
`fuse3` package. Direct local unverified runs use bundled libgit2 and need no Git
executable. Remote runs and `--verify` require native Git; invoking the `git on`
external command also uses Git. The installer needs `curl` and Python 3.8+.

### Install or update

Install the latest stable release, or update using the same command:

```sh
curl -fsSL https://raw.githubusercontent.com/progbeat/git-on/master/install.sh | sh
```

Run as your normal user. The installer verifies SHA-256 checksums and release
metadata, then atomically replaces `~/.local/bin/git-on`. It does not modify shell
profiles. Add the destination to your current shell's `PATH` if needed:

```sh
export PATH="$HOME/.local/bin:$PATH"
git on --version
```

See the [installation guide](docs/releases.md#installing-a-published-binary) to
inspect the script first, pin a version, or choose another directory. Anonymous
downloads require a public repository; [private releases](docs/releases.md#inspect-first-or-use-an-authenticated-private-release)
use your existing GitHub CLI authentication.
For a source installation, see [building from source](docs/releases.md#building-from-source).

## Quick start

Run from an existing Git repository with a commit:

```sh
# List the committed tree, ignoring local changes.
git on HEAD -- ls

# Run an older snapshot (requires at least two commits).
git on HEAD~1 -- ls

# Show the command-line reference.
git on -h
```

Commands start at the selected tree's root, even when invoked from a repository
subdirectory. Use a commit, commit tag, raw tree ID, or `HEAD:path/to/subdirectory`.
A full object ID selects an exact snapshot.

### Allow temporary writes

Commit ignore rules for the files your command creates. For example, a Python
project might use:

```gitignore
.venv/
__pycache__/
.pytest_cache/
```

Only ignore rules in the selected tree grant write access. Tracked files stay
immutable even when they match a rule. See the [locked uv examples](examples/README.md)
for runnable projects.

### Keep results

Use `--keep` (or `-k`) and a command separator to retain generated ignored files:

```sh
git on HEAD:examples/uv-project -k -- uv run --locked main.py
git on HEAD:examples/uv-project --keep -- uv run --locked main.py
```

Put runner options after the revision.
Every command requires `--`: put the revision and runner options before it,
then the command and its literal arguments after it. The command's own `--`,
`--keep` and `-k` are passed unchanged; see the
[syntax reference](docs/usage.md#command-syntax).

After unmounting, git-on prints the retained temporary directory on stderr.
Generated files are under `backing/`; remove the directory when you are done.
Each subsequent run starts fresh. Use ordinary shell redirection to save output,
for example `git on HEAD -- ls > committed-files.txt`.

## Support and limitations

Run a remote revision from any directory, or select a configured remote from
inside its local repository:

```sh
git on https://github.com/OWNER/REPO.git main -- python script.py
git on origin main:experiments -k -- uv run --locked experiment.py
git on ssh://git@HOST/REPO.git signed-release --verify -- ./check.sh
git -c gpg.ssh.allowedSignersFile=/path/to/allowed_signers on HEAD --verify -- ./check.sh
```

Remote runs fetch selected and subsequently demanded objects. They require native
Git 2.45.1+, a server advertising object filters and allowing exact object
requests, and existing HTTPS credentials or OpenSSH authentication. An unlocked
SSH agent needs no passphrase prompt. One selected encrypted identity
authenticates before mounting and lazy fetches reuse that SSH connection.

`--verify` asks native Git to verify the exact commit or annotated tag before
execution. Git controls formats and trust through your effective configuration,
including `git -c`; git-on adds no signer allowlist or trust threshold. Failed
verification forbids execution. A signed tag binds its
peeled tree; `REV:path` selects a directory inside that verified tree. Raw trees
cannot be verified by searching history. Verification proves neither passed
tests nor execution safety. See the [remote guide](docs/remote.md) for limits.

git-on is **not a security sandbox**. Commands retain your user identity,
environment, network access, and access to host files outside the mount. Run only
code you trust.

macOS and Windows are unsupported. Other Linux architectures have no prebuilt
release or CI support guarantee. Containers need `/dev/fuse` and permission to mount.

Git executable bits and symlinks are preserved. Submodules and Git LFS content
fail when accessed; unrelated files remain usable. Git SHA-256 repositories are
unsupported. The mount contains source files without a synthetic `.git` checkout;
Git filters and hooks are not run.

## Documentation

- [Usage and behavior](docs/usage.md): write policy, streams, signals, cache limits, and cleanup.
- [Examples](examples/README.md): locked Python, uv, NumPy, and Numba workloads.
- [Troubleshooting](docs/usage.md#troubleshooting): setup and runtime errors.
- [Benchmarks](tests/benchmark.md): million-file workloads, exact output, and object-demand checks.
- [Releases and installation](docs/releases.md): versioning, verification, and publication.

## Contributing

Start with the [specification](docs/specification.md) and [contributor instructions](AGENTS.md).
The [test guide](tests/README.md) covers local checks and required Linux/FUSE tests.
CI checks Rust, real mounts, callback coverage, seeded stress, performance, and
release packaging. Use the PR's checks to verify its exact revision; the badge
above reports `master`.

Every PR includes a version bump and release notes. Published history lives in
[GitHub Releases](https://github.com/progbeat/git-on/releases).
For a [bug report](https://github.com/progbeat/git-on/issues), include the version,
Linux distribution, Git tree ID, minimal command and ignore rules, expected result,
and actual error. Remove secrets and private source from the report.

## License

git-on is licensed under the [MIT License](LICENSE). Dependencies retain their
respective licenses and copyright notices.
