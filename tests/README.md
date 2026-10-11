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

## Remote transport and native verification

`tests/remote.py` starts disposable HTTPS smart-Git and OpenSSH servers with
fixture certificates, identities, agents and signing keys. Native Git is the
verification oracle. Install Git 2.45.1+, openssh-server, OpenSSL and GnuPG;
no user key or trust store is read or changed by these tests.

```console
cargo build --locked --features test-metrics --example source-probe --bin git-on
python3 tests/remote.py
python3 tests/ssh_regressions.py
python3 tests/remote.py --require-fuse
python3 tests/remote.py --require-fuse --million RemoteTests.test_million_distinct_file_remote_cold_and_warm_bounded_fetch
```

The source probe exercises the production loader without a mount and records
received OIDs, requested objects, request/response bytes and loader metrics.
Tests distinguish listing from stat, repeated reads and large ranges; compare
cold and warm caches; use distinct trees for moving-ref/replacement checks; and
cover native formats/trust, tag chains, inherited Git settings and server faults.
SSH tests count ASKPASS and interactive PTY prompts and authenticated connections.
The million fixture has exactly one million distinct seven-byte blobs at six
digit paths; preparation verifies every leaf before measuring remote demand.

The `rust` CI job runs loader and SSH regressions. `real-fuse` requires real
mounts for remote writes, keep, cancellation, busy/stale recovery, external
`git -c --verify`, and caught transport EIO. `lazy-performance` requires actual
FUSE for the remote million cold no-op/read and warm read. A loader success or
an explicitly skipped local mount test is never a real-FUSE pass. Public GitHub
availability is not required by these deterministic gates.

## Complete FUSE callback inventory

The auditable [callback matrix](../docs/fuse-coverage.md) covers every method of
our pinned `fuser::Filesystem` trait: implemented operations, inherited lifecycle
methods, explicit unsupported replies, and platform exclusions. Its named tests
separate wire-level callback assertions from actual Linux syscall behavior.

```console
python3 tests/check_fuse_coverage.py --self-test
python3 tests/check_fuse_coverage.py
cargo test --locked --test fuse_protocol
```

The guard requires downloaded locked Cargo dependencies. It fails on a changed
trait fingerprint, missing/duplicate method, wrong classification, or missing
named test. It does not infer branch coverage from a test name. The protocol
suite supplies FUSE 7.31 packets to the actual `fuser` session/Overlay dispatcher
over a local socket transport; it is deliberately not described as a kernel
mount. It covers callbacks bypassed by kernel permissions or capability choices,
unsupported defaults, credentials, invalid handles, pagination, and ordinary
backing-store errors. The real-FUSE job separately tests kernel translation and
fallbacks (including locks, polling, seeking, and copying).

## Filesystem mutation matrix

The real-FUSE suite covers these categories separately, with exact byte and
namespace checks after failures rather than accepting an arbitrary exception as
proof of protection:

| Operation | New ignored artifacts | Disallowed new paths | Tracked paths, including ignored names |
| --- | --- | --- | --- |
| Create / exclusive create | Allowed; duplicate exclusive create is `EEXIST` | Rejected without leaving a path | Existing immutable contents preserved |
| Write / read-write open / append | Allowed; offsets and append bytes verified | Creation rejected for each open mode | Rejected, contents and metadata preserved |
| Truncate by path/fd/open | Shrink, zero extension and `O_TRUNC` checked | Create-and-truncate rejected | Path and open truncate, including read-only+truncate, rejected |
| chmod / chown-self / utime | File/fd and directory metadata verified | No inode exists to modify | Rejected for files and directories |
| Rename / replace | File and recursive directory moves, replacement and open-replaced fd checked | Destination rejected; source remains intact | Source moves and destination replacement rejected, including cold lookups |
| Unlink / rmdir | Allowed; nonempty rmdir rejected; open-unlinked fd remains usable | No path can first be created | Rejected |
| Symlink | Create, readlink, write-through, rename and unlink checked | Link name and resolved write destination rejected | Tracked target cannot be changed through an ignored symlink |
| Hardlink | Shared inode, link count, alias writes and final unlink checked | Link destination rejected | Tracked source and destination rejected |

Cases cover ignored directories, ignored filename patterns, nested rules,
negations and tracked files matching those same ignores. Three-level rule
precedence is compared with `git check-ignore --no-index`; directory-only,
anchored, escaped, character-class and unusual-byte rules retain separate
regressions. Runtime-created ignore files cannot expand the pinned policy.
Concurrent readers/writers verify append records and temporary-file rename/delete
cycles; simultaneous mounts verify isolation between runs. None of these tests
claim the mounted command is sandboxed from the host filesystem.

## Lazy access and lifecycle

The ordinary integration binary has no metrics channel. Black-box laziness
regressions use unreachable or deliberately unavailable sibling objects to check
that unrelated content is not visited, while separate workloads exercise lookup,
listing, stat, reads, applicable nested ignore rules, shared-blob correctness,
and large-file chunked/range reads. Unit tests validate cache retention and
object decode reuse. The [benchmark harness](benchmark.md) uses the separate
`test-metrics` build and an explicitly inherited metrics pipe for structural
object-count and cache-bound gates on the full million-distinct-file workload.
It records stdout/stderr and counters externally; it does not use a runtime
manifest. Timing by itself is not a laziness assertion.

Unsupported LFS blobs and submodules must fail on relevant access while ordinary
siblings remain readable; mounting an untouched unsupported subtree must not
require global preflight traversal. Directory name enumeration remains possible.

Other coverage includes pinned snapshots despite dirty index/worktree state;
raw and subtree tree objects; executable modes; literal and non-UTF-8 argv;
binary standard streams and inherited stdin; exit/signal return codes; direct
terminal detection, PTY foreground ownership and stop/continue handling; missing
executables; forwarded signals; default temporary-layer removal; `TMPDIR`
selection; mandatory command separators; `--keep`/`-k` on either side of the
revision; exact argument forwarding after the first separator; rejected old direct
command syntax, extra operands and unknown options without runtime state; retention
after unmount; absence of built-in logs, manifests,
and output directories; normal and SIGKILL recovery cleanup; refused cleanup of
active or deliberately retained state; retained busy mounts and explicit
recovery; and locked PEP 723/uv project imports with Numba cache reuse. Concurrent
runs verify isolation and normal mount/Git-pin cleanup. Example artifact tests
retain the writable layer with `--keep` and inspect its `backing/__pycache__/` directly.

## Randomized filesystem differential stress

See [stress.md](stress.md) for bounded seeded real-mount operation generation,
exact namespace/content/metadata/error comparison, and trace replay. CI requires
a real FUSE device for this suite; host-only oracle self-tests are separate.


## Installer acceptance

`python3 tests/test_installer.py` runs offline latest/pinned release selection,
repeat installation, updates, checksum/download failure preservation, unsafe
archives/destinations, platform checks, and atomic replacement regressions.
A fake GitHub CLI fixture exercises the actual `sh install.sh` entrypoint without
network access; unit-level transport fixtures cover the anonymous HTTPS path.

The Ubuntu 22.04 `package` job passes the actual release assets to
`python3 tests/test_installer.py --dist dist --install-dir "$PWD/installed"`, then
runs the complete real-FUSE integration suite against `installed/git-on`.
Installer fixtures are not a substitute for that packaged-binary mount test.
