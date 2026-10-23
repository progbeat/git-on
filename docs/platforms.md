# Platform prerequisites and release-readiness verification

The 1.3 implementation builds native Apple Silicon macOS and Linux x86_64.
A build or materialization test is not a real FUSE mount pass. Record the exact
commit, OS/kernel, backend version, privileges and commands for each gate.

## Apple Silicon macOS

The deployment target is macOS 13+. The backend is macFUSE's libfuse2 compat25
mount API with FUSE ABI 7.31/macFUSE 4 compatibility. Use official macFUSE 4.10.2
or a compatible later release providing `/usr/local/lib/libfuse.2.dylib` and
`/Library/Filesystems/macfuse.fs/Contents/Resources/mount_macfuse`.
The binary loads this library at runtime, not an arbitrary library search path.
No driver or SDK is required merely to build or use the explicit-all fallback.
Intel macOS and FUSE-T are not part of this support target.

Required mount options are owner-only access, default kernel permissions, nosuid
and nodev, with read-only mounting for policy none. Mount inventory/identity use
getmntinfo; cleanup uses a normal `/sbin/umount` and refuses busy/foreign mounts.
No forced/lazy teardown, extension loading or security-setting adjustment exists.

On the development Mac (arm64 macOS 26.3.1(a)), macFUSE/FUSE-T was absent in
standard paths. Actual mount checks therefore remain BLOCKED, not skipped passes.
The exact prerequisite is an owner-approved installation and usable official
macFUSE libfuse2/kernel backend on an Apple Silicon Mac, with mount permission.
If its installation requires system extension approval/reboot/security changes,
those require the owner's separate decision. This PR does not authorize them.

After those prerequisites are independently provided, build/install the packaged
binary and perform real mounts for all four policies: read committed bytes and
links/modes; deny tracked mutation for ignored/untracked; deny all writes for
none; permit disposable tracked changes for all; verify source/index identity,
command exit/status, SIGINT/TERM and terminal stop/continue, busy-mount recovery
and complete normal cleanup. Run from a subdirectory and a selected subtree.
Start with `python3 tests/macos_mounts.py --binary /absolute/path/git-on`; missing
prerequisites fail rather than skip. Extend with `tests/write_policy.py`'s scenarios and portable scenarios from
`tests/integration.py`; Linux /proc/protocol/fusermount-specific scenarios need
macOS counterparts and must never be relabelled as Mac evidence.

The inherited macOS-specific FUSE exchange/getxtimes/setvolname callbacks return
ENOSYS. Ordinary host metadata and platform kernel fallbacks can differ. Remote
per-request interruption currently uses Linux /proc task signal inspection;
macOS whole-command signals are implemented, but remote requester-specific
interruption requires real backend verification before a support claim.

## Linux kernel passthrough

Linux kernel passthrough requires FUSE protocol 7.40 (Linux 6.9+) and effective
CAP_SYS_ADMIN for backing-file registration. Normal users without it keep
ordinary FUSE I/O; never run `sudo git on` merely to enable this optimization.
The runner neither acquires capabilities nor elevates the child.

Only command-created physically backed regular files qualify. Git blobs and
tracked copy-up remain ordinary FUSE. Registrations are shared by concurrent
handles and released with them. Logical modes/owners/link counts stay overlay
metadata; data size, allocation and timestamps refresh from backing files.

```sh
cargo build --locked --features test-metrics
python3 tests/passthrough.py --binary target/debug/git-on --expect=fallback
python3 tests/passthrough_lifecycle.py --binary target/debug/git-on --expect=fallback
# On a separately provisioned capable host/daemon identity, without elevating child:
python3 tests/passthrough.py --binary target/debug/git-on --expect=enabled
python3 tests/passthrough_lifecycle.py --binary target/debug/git-on --expect=enabled
```

Enabled verification requires successful kernel backing registration and zero
userspace generated-file read/write callbacks, including writes, extend/stat,
close/reopen/read, truncate/stat/read, rename/unlink, compiled execution and mmap
page faults after unlink while the daemon remains alive. Fallback requires zero
registrations and positive userspace write callbacks. The production binary has
no metrics channel; the existing opt-in test descriptor is close-on-exec.

A normal unmount while an executable/mapping or cwd is busy is expected to fail;
a lazy unmount detaches a namespace but does not prove release of all kernel
references. Daemon-exit/subsequent-page-fault guarantees and measured benefit
must be recorded on the tested kernel. Those destructive lifecycle experiments
remain an explicit verification gate, not a claim inferred from passthrough.
The lifecycle harness reports actual subsequent page-fault/descriptor outcomes
after physical backing-name unlink, busy normal unmount, test-only lazy detach
and daemon SIGKILL, then checks explicit recovery and source isolation. Only
its own unique disposable mount is touched. Detach/daemon-exit observations are
not treated as safety guarantees; abnormal outcomes are reported explicitly.
The execution contract still waits for the command, stops its remaining group,
normally unmounts and checks cleanup. There is no early-unmount/detached mode.

## Native artifacts and integrity

CI builds/probes Linux x86_64 on Ubuntu 22.04 and native arm64 on macos-14,
verifies cross-job SHA-256 inventories, and exercises both generated installer
paths. macOS hosted CI does not pretend to supply a usable macFUSE driver.
Actual macFUSE and capable-kernel lifecycle evidence are prerequisites for
marking the milestone PR ready; no published release is needed to test its
packaged artifacts. See [releases](releases.md).

### Hosted macOS CI feasibility

Standard GitHub-hosted `macos-14` is an Apple Silicon VM, and the native/macOS
suite in `ci.yml` executes Rust, CLI, materialized filesystem and installer
checks, rather than stopping at compilation. The hosted image does not provide
the approved macFUSE kernel backend required by this port. Installing/loading it
or altering the VM's security policy has not been authorized or attempted.
Upstream's [installation guide](https://github.com/macfuse/macfuse/wiki/Getting-Started)
explicitly states that recent macOS cannot load third-party kernel extensions
inside a VM. Installing the package on this hosted VM would not verify the VFS
backend.

macFUSE 5 also offers an explicit `backend=fskit` on newer macOS. Its upstream
[backend documentation](https://github.com/macfuse/macfuse/wiki/FUSE-Backends)
currently lists unimplemented kernel-handled mount options. This is not yet a
verified substitute for owner-only/default-permission/nosuid/nodev behavior;
the port does not silently select it or remove required options to get a green
test. A suitably provisioned Apple Silicon/macFUSE host is still needed for the
non-skipping `tests/macos_mounts.py` suite. This remains a real CI/acceptance gap,
not a claim that hosted native tests passed real Mac mounts.

### Concrete owner prerequisite

For this port, the proposed test dependency is the official
[macFUSE 5.3.3](https://github.com/macfuse/macfuse/releases/tag/macfuse-5.3.3)
DMG's `Install macFUSE.pkg`, using the default VFS/kernel backend on a physical
Apple Silicon Mac. An already approved installation is sufficient. On a first
installation, upstream requires Recovery's Reduced Security and permission for
user-managed kernel extensions, followed by extension approval and restart.
Installing the package alone does not authorize these separate owner actions.
Neither disabling SIP nor disabling Gatekeeper is needed. No such installation,
approval, security change or restart has been performed by this PR.

### Existing Linux host evidence

Independent Brick validation at `71eab9f0ea93855cb6f63496955e251ac97accf0`
ran 55 real-FUSE tests and eight write-policy tests successfully on kernel
6.8.0-147-generic, as an ordinary user. The fallback passthrough scenario passed
with zero registrations, eight generated read callbacks and 45 write callbacks,
including mmap after unlink and compiled execution. Its existing two mounts and
clean checkout were preserved. This does not verify enabled kernel passthrough.

The existing `ubuntu-latest` real-fuse job at `23f5fca` reported kernel
6.17.0-1022-azure, `CONFIG_FUSE_PASSTHROUGH=y`, usable `/dev/fuse`, UID 1001,
zero effective/permitted/ambient capabilities and `NoNewPrivs=0`.
See [the read-only prerequisite probe](https://github.com/progbeat/git-on/actions/runs/39958126361/job/120428991862).
This same runner supplies the kernel prerequisite without changing Brick or
creating infrastructure. The owner approved an isolated CI experiment: copy the
test binary into a private runner temporary directory, grant only that copy
`cap_sys_admin=ep`, execute the enabled suites as the ordinary runner user, then
remove the capability and copy with an EXIT trap. The command/worker tests
assert ordinary UID and zero effective/permitted/ambient capabilities. The
checkout and packaged binaries receive no capability. Actual enabled results
must still be checked; defining this experiment is not a passed mount test.

On that runner at `897088d`, the ordinary-FUSE lifecycle experiment observed
normal unmount failing with EBUSY while a worker held cwd/mmap. A test-only lazy
detach removed the namespace mount but kept the worker's existing data usable;
daemon SIGKILL left the mount disconnected and descriptor reads returned
ENOTCONN. A previously cached mmap tail survived a MADV_DONTNEED page-table
discard in all three cases. Each case completed checked cleanup. These cached
page observations do not prove future uncached data access: the permanent
harness additionally probes a previously unread sparse page and records any
SIGBUS/errno separately. Enabled-passthrough observations are still required.
