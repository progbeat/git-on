# Platform requirements and verification

Git-on 1.3.0 builds Linux x86_64 and Apple Silicon CLI binaries. Linux mounts use
registry fuser0.18.0, without a fork or vendored source. A usable `/dev/fuse` and
`fusermount3` are required; optional passthrough does not change that requirement.

## Apple Silicon

macOS13+ supports CLI startup and explicitly selected `--write-policy=all`
materialization. Default `ignored`, `untracked` and `none` policies fail clearly:
1.3.0 provides no supporting Mac mount backend. macOS mounts are deferred to
[issue #12](https://github.com/progbeat/git-on/issues/12), which remains open outside
this milestone. No FSKit/macFUSE adapter, app bundle, activation or security change
is part of this release. The same console installer installs the CLI binary.

The Mac CI job runs strict clippy, Rust tests, eleven CLI cases, materialized
execution and installer tests. Native packaging also executes the extracted
binary and unmodified generated installer. The standard installer can skip
checksum verification when its utility is absent; build-only success is not
substituted for behavior tests. Actual Mac mounts are not claimed.

On the owner's Mac27.0.1 (26A434), SDK26.5, the reduced implementation passed
58 core, 12 binary and 10 remote-source Rust tests, strict metrics clippy,
seven materialized-execution and eleven CLI cases. Local tests use disposable
fixtures; no extension is installed or activated.

## Linux passthrough

Passthrough requires Linux6.9+/FUSE7.40 and CAP_SYS_ADMIN for registration. It is
optional: ordinary users or unsupported kernels retain FUSE userspace I/O, never
materialization solely because this optimization is unavailable. Only physically
backed command-generated regular files qualify; Git blobs and tracked copy-up do
not. The child receives no new privileges; never recommend sudo git on for this.

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

CI exercises Linux real mounts/performance/MSRV and Mac CLI/materialized behavior,
both native archives and corrected generated installers. Exact final-head CI and
Copilot review remain readiness gates; COMMENTED/no findings is not APPROVED.
