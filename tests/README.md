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

Separate fresh-mount workloads perform no access, list one directory, stat one
leaf, look up a missing leaf and read one leaf in a 10,000-distinct-blob fixture.
Manifest counters must show only necessary ancestor trees/header reads and zero
unread payloads. A write under nested ignore rules loads only its two ancestor
ignore files while twelve unrelated policy subtrees remain untouched. Distinct
pathnames sharing one blob test object-cache reuse;
a 16 MiB chunked/range read tests one decode per open file. Cache retained-byte
counts must stay within the reported limits. The [benchmark harness](benchmark.md)
uses the full million-distinct-file acceptance workload with the same structural
counter gates. Timing by itself is not a laziness assertion.

Unsupported LFS blobs and submodules must fail on relevant access while ordinary
siblings remain readable; mounting an untouched unsupported subtree must not
require global preflight traversal. Directory name enumeration remains possible.

Other coverage includes pinned snapshots despite dirty index/worktree state;
raw and subtree tree objects; executable modes; literal and non-UTF-8 argv;
binary stream logs and inherited stdin; exit/signal manifests; PTY foreground
ownership and stop/continue handling; missing executables; forwarded signals;
normal and SIGKILL recovery cleanup; refused cleanup of active runs; retained
busy mounts and explicit recovery; and locked PEP 723/uv project imports with
Numba cache reuse. Successful concurrent runs verify removal of mount, backing
storage and Git pins while keeping requested output artifacts.
