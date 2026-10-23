# git-on

[![CI (master)](https://github.com/progbeat/git-on/actions/workflows/ci.yml/badge.svg?branch=master&event=push)](https://github.com/progbeat/git-on/actions/workflows/ci.yml?query=branch%3Amaster+event%3Apush)
[![Release](https://img.shields.io/github/v/release/progbeat/git-on)](https://github.com/progbeat/git-on/releases)
[![Rust 1.89+](https://img.shields.io/badge/Rust-1.89%2B-blue?logo=rust&logoColor=white)](Cargo.toml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Run commands against a Git revision without checking it out. Git objects and your
working copy stay unchanged; choose read-only, ignored-only, or temporary writes
to any snapshot path.

![Build and run the original SL in a temporary Git snapshot](https://raw.githubusercontent.com/progbeat/git-on/943434c1f102a0efc626cafd40ae6245002fa392/demo.webp)

[Installation](#installation) · [Quick start](#quick-start) ·
[Documentation](#documentation)

- **Pinned source.** Run committed code without changing your working tree or
  index. The selected tree stays fixed for the entire command.
- **Lazy access.** Read files and directories on demand through FUSE, without
  unpacking or scanning the whole repository at startup.
- **Disposable writes.** Each run gets a fresh writable layer. The default allows
  ignored paths; `--write-policy=all` also allows changing source files.
  Cleanup is automatic; save required artifacts to explicit external paths.

## Installation

The 1.3.0 pipeline builds Linux x86_64 (glibc 2.35+) and native Apple Silicon
macOS (macOS 13+). Linux FUSE requires `/dev/fuse` and `fusermount3`; macOS
mounts use our native Apple FSKit extension in GitOn.app (macOS 26.4+). Real native FSKit acceptance is a
release-readiness prerequisite; see [platform validation](docs/platforms.md).
When FUSE is unavailable, only explicit `--write-policy=all` can use a fresh,
eagerly materialized directory. Other policies fail without being weakened.
Native Git is required on PATH; remote execution requires Git 2.45.1+.

### Install or update

Install the latest stable release, or update using the same command:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://sh.progbeat.dev/git-on | sh
```

Run as your normal user. The thin bootstrap downloads a complete release
installer over HTTPS, then the generated installer verifies the selected archive's
SHA-256 before installing `~/.local/bin/git-on`. The installer accepts only 1.3.0
and later, and never modifies shell profiles. Add the destination to PATH:

```sh
export PATH="$HOME/.local/bin:$PATH"
git on --version
```

See the [installation guide](docs/releases.md#installing-a-published-binary) to
inspect the script first, pin a version, or choose another directory. Anonymous downloads require a public repository; `--github-cli` is explicitly removed.
See the installation guide for authenticated manual download instructions.
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

One positional operand is passed unchanged to native Git for local revision
resolution. Two operands are passed unchanged to `git fetch` as its repository
and refspec, including configured remote names, local paths, URLs and refspec
forms that Git accepts. git-on does not impose its own repository or revision
syntax. Git decides whether an abbreviated object ID or another expression is
valid in that context; the selected object is then pinned for the run.

### Allow temporary writes

Use `--write-policy=none|ignored|untracked|all` before `--`. `ignored` is the
default: commit ignore rules for the files your command creates. For example,
a Python project might use:

```gitignore
.venv/
__pycache__/
.pytest_cache/
```

Only ignore rules in the selected tree grant write access. Tracked files stay
immutable even when they match a rule. See the [locked uv examples](examples/README.md)
for runnable projects.

`untracked` allows new paths independently of ignore rules, while protecting
every entry in the selected Git tree and its parent directories. It uses that
tree, not your index or working copy, to decide which paths are new.

For a completely read-only snapshot, use `none`. To compile or edit any snapshot
file in a disposable copy-on-write layer, use `all`:

```sh
git on HEAD --write-policy=none -- ls
git on HEAD --write-policy=untracked -- sh -c 'printf result > result.txt'
git on HEAD --write-policy=all -- sh -c 'cc source.c -o program && ./program'
```

Every mode preserves the original Git objects and working copy. This policy
governs the mounted tree; commands can still access host paths outside it.

### Save results

Execution snapshots are disposable. `--keep` and `-k` were removed in 1.3.0;
write logs and artifacts to explicit external paths:

```sh
git on HEAD -- ls > committed-files.txt
git on HEAD --write-policy=all -- sh -c 'make && cp program /absolute/path/artifacts/program'
```

Runner options go before the first `--`. Everything after it belongs to the
child, including `--keep`, `-k`, empty arguments and additional separators.

## Support and limitations

Run a remote revision from any directory, or select a configured remote from
inside its local repository:

```sh
git on https://github.com/OWNER/REPO.git main -- python script.py
git on origin main -- python script.py
git on ssh://git@HOST/REPO.git signed-release --verify -- ./check.sh
git -c gpg.ssh.allowedSignersFile=/path/to/allowed_signers on HEAD --verify -- ./check.sh
```

Remote runs fetch selected and subsequently demanded objects. They require native
Git 2.45.1+, a server advertising object filters and allowing exact object
requests, and existing HTTPS credentials or OpenSSH authentication. An unlocked
SSH agent needs no passphrase prompt. One selected encrypted identity
authenticates before mounting and lazy fetches reuse that SSH connection.

For a local-path source, first enable those capabilities in the source repository:

```sh
git -C ../another-repo config uploadpack.allowFilter true
git -C ../another-repo config uploadpack.allowAnySHA1InWant true
git on ../another-repo main -- ls
```

The one-operand local mode (`git on HEAD -- ls`) reads its repository directly
and needs no upload-pack setup; see the [remote guide](docs/remote.md#object-demand-and-cache-lifetime).

`--verify` asks native Git to verify the exact commit or annotated tag before
execution. Git controls formats and trust through your effective configuration,
including `git -c`; git-on adds no signer allowlist or trust threshold. Failed
verification forbids execution. A signed tag binds its peeled tree. A local
`REV:path` resolves to a tree, which has no signature; to verify its commit,
select `REV --verify` and change directory inside the command. Raw trees cannot
be verified by searching history. With two operands, a colon is native
fetch refspec syntax, such as `main:refs/heads/demo` in git-on's private repository;
it does not select a subtree. Verification proves neither passed
tests nor execution safety. See the [remote guide](docs/remote.md) for limits.

git-on is **not a security sandbox**. Commands retain your user identity,
environment, network access, and access to host files outside the mount. Run only
code you trust.

Windows and Intel macOS have no support guarantee. Other Linux architectures have
no prebuilt release. Containers need `/dev/fuse` and mount permission for FUSE.
Apple Silicon/FSKit validation requirements and known limits are documented
in [platforms](docs/platforms.md).

Git executable bits and symlinks are preserved. Submodules and Git LFS content
fail when accessed; unrelated files remain usable. Git SHA-256 repositories are
unsupported. The mount contains source files without a synthetic `.git` checkout;
reading the snapshot does not run checkout filters or worktree hooks. Native
fetches honor configured Git hooks, which run as ordinary user programs outside
the mount. Fetch destination refs are written only in git-on's private repository.

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

CLI source, runtime dependency and binary build changes include a version bump
and release notes. Documentation, GIFs, tests, standalone examples and installer
changes alone keep the binary version; required CI still runs. Published history lives in
[GitHub Releases](https://github.com/progbeat/git-on/releases).
For a [bug report](https://github.com/progbeat/git-on/issues), include the version,
Linux distribution, Git tree ID, minimal command and ignore rules, expected result,
and actual error. Remove secrets and private source from the report.

## License

git-on is licensed under the [MIT License](LICENSE). Dependencies retain their
respective licenses and copyright notices.
