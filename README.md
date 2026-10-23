# git-on

[![CI (master)](https://github.com/progbeat/git-on/actions/workflows/ci.yml/badge.svg?branch=master&event=push)](https://github.com/progbeat/git-on/actions/workflows/ci.yml?query=branch%3Amaster+event%3Apush)
[![Release](https://img.shields.io/github/v/release/progbeat/git-on)](https://github.com/progbeat/git-on/releases)
[![Rust 1.89+](https://img.shields.io/badge/Rust-1.89%2B-blue?logo=rust&logoColor=white)](Cargo.toml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Run commands from a Git tree pinned by hash, or from revisions accepted by your
configured Git signature policy.

Reuse a tree hash for the same source contents across runs. Use `--verify` to
require a signed commit or annotated tag before execution.

Each run starts from the selected tree, with temporary writes discarded afterward.
Your working tree and index stay unchanged.

![Build and run SL in a temporary Git snapshot](https://raw.githubusercontent.com/progbeat/git-on/943434c1f102a0efc626cafd40ae6245002fa392/demo.webp)

[Installation](#installation) · [Usage](#usage) · [Documentation](#documentation)

## Installation

Install on [Linux x86_64 or Apple Silicon macOS](docs/platforms.md):

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://sh.progbeat.dev/git-on | sh
```

The default location is `~/.local/bin`. If `git on` is not found, restart your
shell or add that directory to PATH in the current shell (sh, bash or zsh):

```sh
export PATH="$HOME/.local/bin:$PATH"
```

Check the installation:

```sh
git on --version
```

You need Git. On Linux, install [FUSE](docs/platforms.md) to use the default write
policy. See the [installation guide](docs/releases.md#installing-a-published-binary)
for custom install locations and other setup options.

## Usage

### Pin a source tree

Resolve your chosen revision to its tree hash once. On Linux:

```sh
tree=$(git rev-parse 'HEAD^{tree}')
git on "$tree" -- ls
```

On macOS, use `--write-policy=all`; other write policies are not supported yet:

```sh
tree=$(git rev-parse 'HEAD^{tree}')
git on "$tree" --write-policy=all -- ls
```

Record the value of `$tree` and pass it to later runs to select the same source
contents. Commands start at the snapshot's root. Everything after `--` belongs
to your command.

### Run a verified revision

[Configure Git's trusted keys and verification policy](docs/remote.md#native-signature-verification)
for your review or QA process, then require verification before running:

```sh
git on main --verify --write-policy=all -- python script.py
```

`--verify` checks the selected commit's signature. With an annotated tag, it
checks the tag's signature instead. Verification failure stops execution.

### Build and test the pinned tree

Allow temporary writes to any snapshot file with `--write-policy=all`:

```sh
git on "$tree" --write-policy=all -- cargo build
git on "$tree" --write-policy=all -- cargo test
```

By default on Linux, tracked files stay read-only. New files and directories
must be ignored by the selected revision's committed `.gitignore` rules. Use
`all` to edit tracked files or write without ignore rules. See
[write policies](docs/usage.md#filesystem-behavior) for more control.

### Choose another revision

Branches, tags and commits also work. For a one-off run at the previous commit
or another branch:

```sh
git on HEAD~1 --write-policy=all -- python script.py
git on feature-branch --write-policy=all -- cargo test
```

### Run a remote project

Supply a repository URL and a revision. Replace the URL with your project:

```sh
git on https://github.com/OWNER/REPO.git main --write-policy=all -- cargo test
```

A configured remote name, such as `origin`, also works. See the
[remote guide](docs/remote.md) for fetching and signature verification.

For all command-line options, run:

```sh
git on -h
```

**git-on is not a security sandbox.** Run only code you trust.

## Documentation

- [Usage, write policies and troubleshooting](docs/usage.md)
- [Remote repositories and signature verification](docs/remote.md)
- [Runnable examples](examples/README.md)
- [Platform requirements](docs/platforms.md)
- [Contributing](AGENTS.md)

## License

[MIT](LICENSE)
