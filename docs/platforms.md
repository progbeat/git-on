# Platform requirements and verification

Git-on builds Linux x86_64 and Apple Silicon CLI binaries. Linux mounts use
registry fuser 0.18.0. A usable `/dev/fuse` and `fusermount3` are required;
optional passthrough does not change that requirement.

HTTPS uses the system libcurl and its TLS and HTTP/2 libraries. The Linux
binary loads the distribution's `libcurl.so.4` at startup. On Ubuntu the
`curl` package installs this dependency. macOS supplies libcurl as part of
the operating system. The optimized transport requires TLS and HTTP/2 support
in libcurl. Native Git handles HTTPS on builds that lack either capability.

## Apple Silicon

The CLI runs on macOS 13 or later. macOS 26 and later can mount a lazy snapshot
with any write policy using the built-in NFSv4.1 client and a private local
socket. This needs an ordinary user account and a writable temporary directory.
The same console installer installs the single executable.

On earlier versions, `--backend=auto` uses a temporary copy when
`--write-policy=all` is explicit. The same fallback is available when the NFS
mount is unavailable and any partial mount has been cleaned up.
`--backend=nfs` requires a real mount. See the
[NFS profile and lifecycle](nfs.md).

The macOS 26 CI job requires real NFS mounts for the filesystem and write-policy
fixtures shared with FUSE. It also runs selected upstream pynfs tests
over the Unix socket. macOS 15 CI runs fallback cases with the extracted native
binary. Both jobs test the revision under review.

## Linux passthrough

Passthrough requires Linux6.9+/FUSE7.40 and CAP_SYS_ADMIN for registration. It is
optional: ordinary users or unsupported kernels retain FUSE userspace I/O, never
materialization solely because this optimization is unavailable. Physically
backed command-generated files and private copies of tracked files can qualify.
The regular file must already have writable backing when opened. Ordinary
handles and mappings must be released before starting passthrough on that inode.
The child receives no new privileges.

```sh
cargo build --locked --features test-metrics
python3 tests/passthrough.py --binary target/debug/git-on --expect=fallback
python3 tests/passthrough_lifecycle.py --binary target/debug/git-on --expect=fallback
# Separately provisioned capable daemon identity; child remains unprivileged:
python3 tests/passthrough.py --binary target/debug/git-on --expect=enabled
python3 tests/passthrough_lifecycle.py --binary target/debug/git-on --expect=enabled
```

Require actual backing registrations and zero userspace generated read/write
callbacks for enabled tests. Verify metadata/timestamps/allocation/modes, concurrent
handles, reopen/truncate, rename/unlink, executable and mmap behavior. Lifecycle
experiments record physical-name unlink, busy normal unmount, test-only lazy detach
and daemon exit, including unread page faults; they do not authorize early teardown.

[CI at b07e786](https://github.com/progbeat/git-on/actions/runs/39986005244)
verified ordinary mounts and enabled passthrough on6.17.0-1022-azure, UID1001,
with nine registrations and zero generated read/write callbacks. The fallback
reported zero registrations and ten read/46 write callbacks. Both paths verified
path timestamps after the last handle closes, metadata/reopen/truncate, mmap and
compiled execution. Normal unmount was busy; after daemon exit enabled mmap read
an unread sparse page while descriptor read failed ENOTCONN; fallback faulted
SIGBUS. All six lifecycle cases completed checked cleanup. The owner-approved
capability was confined to a private disposable CI test copy, then revoked and
the copy deleted; the child asserted zero effective/permitted/ambient capabilities.

Independent Brick ordinary-FUSE validation at71eab9f passed55 mount and eight
policy cases plus fallback, preserving existing mounts/checkout. Brick's older
kernel does not establish enabled passthrough. A missing /dev/fuse in the saved
Linux container is a failed prerequisite, never a passed real-mount test.

CI exercises Linux FUSE, Rust 1.89, macOS NFS and materialized behavior,
both native archives and the generated installers. Exact final-head CI and
a cold PR review are readiness gates.
