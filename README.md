# git-on

[![Release](https://img.shields.io/github/v/release/progbeat/git-on)](https://github.com/progbeat/git-on/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Run a command against a Git revision without checking it out. Your working tree
and index stay unchanged; temporary changes disappear when the command finishes.

![Build and run SL in a temporary Git snapshot](https://raw.githubusercontent.com/progbeat/git-on/943434c1f102a0efc626cafd40ae6245002fa392/demo.webp)

## Install

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://sh.progbeat.dev/git-on | sh
```

If `git-on` is not on PATH:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

You need Git. Linux mounts also need FUSE (`/dev/fuse` and `fusermount3`).
On Apple Silicon macOS, use `--write-policy=all`: mounts and protected write
policies are not supported yet. See [platform requirements](docs/platforms.md).

## Run

From a repository with a commit:

```sh
git on HEAD -- ls
git on HEAD~1 -- python script.py
git on -h
```

The command starts at the selected tree's root. Put git-on options before `--`;
every argument after it belongs to your command.

To allow temporary edits to any snapshot file:

```sh
git on HEAD --write-policy=all -- sh -c 'make && ./program'
```

On macOS, this copies the whole tree before execution. Linux prefers FUSE and
uses that copy only when a supported mount backend is unavailable.

Choose a write policy with `--write-policy`:

| Policy | Allowed changes |
| --- | --- |
| `ignored` (default) | New paths ignored by rules committed in the selected tree |
| `untracked` | New paths, while protecting the selected tree |
| `none` | No changes |
| `all` | Temporary changes to any snapshot path |

Tracked files remain protected under `ignored`, even if an ignore rule matches.
Write required results outside the temporary tree:

```sh
git on HEAD -- ls > committed-files.txt
```

You can also run a remote revision or require signature verification:

```sh
git on https://github.com/OWNER/REPO.git main -- python script.py
git on signed-tag --verify -- ./check.sh
```

Remote execution needs a compatible Git server and existing authentication.
`--verify` uses Git's configured signature policy. See the [remote guide](docs/remote.md).

## Limits and documentation

Commands retain access to your host files, network and credentials. **git-on is
not a security sandbox; run only code you trust.**

The snapshot has no `.git` checkout. Submodules and Git LFS content are unsupported.
Save logs and artifacts explicitly: snapshots are disposable.

- [Usage and troubleshooting](docs/usage.md)
- [Runnable examples](examples/README.md)
- [Platform requirements](docs/platforms.md)
- [Installation details](docs/releases.md#installing-a-published-binary)
- [Contributing](AGENTS.md) and [tests](tests/README.md)

Licensed under the [MIT License](LICENSE). Dependencies retain their own notices.
