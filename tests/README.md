# Integration tests

```console
cargo build --locked
python3 tests/integration.py --binary target/debug/git-on --require-fuse --uv
```

Requirements: Linux, Git, Python 3.11+, `uv` (CI pins 0.8.22), a usable `/dev/fuse`,
and `fusermount3`. Install the distribution's `fuse3` package. The uv examples
need network access on a cold package cache.

The stdlib unittest harness creates real temporary Git repositories and launches
the compiled binary. It never mocks FUSE, the Git object store, or child processes.
`--require-fuse` fails immediately if the device/helper is missing; mount failures
fail actual assertions. CI always passes this option in its separate real-FUSE
job, along with `--uv`. Tests are not run as root.

For local development where FUSE is unavailable:

```console
python3 tests/integration.py --binary target/debug/git-on CliTests
```

Running the full suite without `--require-fuse` explicitly reports mount-dependent
tests as skipped; this is not evidence that the filesystem works. `--uv` enables
the dependency integration tests and must not be omitted when claiming complete
coverage.

Coverage includes pinned Git snapshots despite dirty index/worktree state; raw
and subtree tree objects; immutable data and metadata; tracked files within
ignored directories; negative, nested, anchored and escaped ignore patterns
compared against Git; runtime ignore changes; link and rename attacks; literal
and non-UTF-8 argv; binary stream logs and inherited stdin; exit/signal manifests;
forwarded signals; cleanup of normal and SIGKILL-interrupted runs; simultaneous
run isolation; and locked PEP 723/uv project imports and Numba cache reuse.
