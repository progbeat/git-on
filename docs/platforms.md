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
# On a separately provisioned capable host/daemon identity, without elevating child:
python3 tests/passthrough.py --binary target/debug/git-on --expect=enabled
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

macFUSE 5 also offers an explicit `backend=fskit` on newer macOS. Its upstream
[backend documentation](https://github.com/macfuse/macfuse/wiki/FUSE-Backends)
currently lists unimplemented kernel-handled mount options. This is not yet a
verified substitute for owner-only/default-permission/nosuid/nodev behavior;
the port does not silently select it or remove required options to get a green
test. A suitably provisioned Apple Silicon/macFUSE host is still needed for the
non-skipping `tests/macos_mounts.py` suite. This remains a real CI/acceptance gap,
not a claim that hosted native tests passed real Mac mounts.
