# git-on

[![Release](https://img.shields.io/github/v/release/progbeat/git-on)](https://github.com/progbeat/git-on/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Run a command against a Git revision without checking it out. Your working tree
and index stay unchanged; temporary changes disappear when the command finishes.

![Build and run SL in a temporary Git snapshot](https://raw.githubusercontent.com/progbeat/git-on/943434c1f102a0efc626cafd40ae6245002fa392/demo.webp)

[Installation](#installation) · [Usage](#usage) · [Documentation](#documentation)

## Installation

Install on Linux or Apple Silicon macOS:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://sh.progbeat.dev/git-on | sh
```

You need Git. On Linux, install [FUSE](docs/platforms.md) to use the default write
policy. See the [installation guide](docs/releases.md#installing-a-published-binary)
for setup and custom install locations.

## Usage

On macOS, use `--write-policy=all` (see the build example). Other write policies
currently require Linux with FUSE.

Run a command at a revision in your repository:

```sh
git on HEAD -- ls
git on HEAD~1 -- python script.py
```

Build an earlier revision, allowing temporary writes:

```sh
git on HEAD~1 --write-policy=all -- cargo build
```

Commands start at the snapshot's root. Everything after `--` belongs to your
command. For more options, run:

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
