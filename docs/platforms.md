# Platform requirements and verification

Git-on builds Linux x86_64 and native Apple Silicon binaries. Linux uses registry
fuser0.18.0 with no fork. macOS uses our direct Apple FSKit extension in GitOn.app,
without macFUSE/MFMount or a kernel extension. Git snapshot, overlay and policy
logic are shared; platform adapters translate operations and manage mounts.

## Apple Silicon

CLI startup and explicit-all materialization retain macOS13+. The native FSKit
bundle targets26.4+, using Apple's public read-only mount API. Build it with
an SDK containing that API:

```sh
python3 scripts/build_macos_bundle.py
```

This produces unsigned target/native/GitOn.app and GitOnFS.entitlements. Building
never installs, signs, registers, activates or changes security settings. Actual
mounts require usable signing, bundle registration and activation of GitOn's
user-space file-system extension. The final installer must provision the CLI
and its companion in one command; manual app installation is not the finished
installation contract. Per-user placement/activation integration is still under
verification. /Applications is only the preliminary local test path. A new signing
credential or security bypass is not an implicit prerequisite or authorized action.
The owner must approve any test installation/activation separately.

Run actual acceptance against the extracted/installed CLI:

```sh
python3 tests/macos_mounts.py --binary /absolute/path/git-on
```

Missing prerequisites must fail, not count as skipped passes. Verify all write
policies, bytes/links/executable modes, generated I/O, metadata, compiled execution,
mmap, source isolation, cross-user access, statuses/signals and terminal job control,
busy recovery and cleanup. The runtime checks actual nosuid/nodev/read-only flags
before launching the command. Checked cleanup records filesystem ID, owner and
source through getmntinfo; it does not trust a path alone. No forced/lazy product
unmount or detached execution exists.

The owner's Mac was verified as27.0.1 (26A434), SDK26.5. Native compilation and
57 shared Rust unit tests passed, followed by seven real materialized-execution
and eleven CLI cases. Actual direct FSKit mounting remains unverified pending
approved bundle signing/installation/activation. Those results are not mount passes.
See [the native assessment](fskit-assessment.md), including the per-file fsync
limitation: persistent databases belong outside the disposable tree.

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

[CI at the preceding checkpoint](https://github.com/progbeat/git-on/actions/runs/39977099308)
verified ordinary mounts and enabled passthrough on6.17.0-1022-azure, UID1001,
with eight registrations and zero generated callbacks. Normal unmount was busy;
after daemon exit enabled mmap read an unread sparse page while descriptor read
failed ENOTCONN; fallback faulted SIGBUS. All cases completed checked cleanup.
The owner-approved capability was confined to a private disposable CI test copy,
then revoked and the copy deleted. New registry/core changes need new-head CI.

Independent Brick ordinary-FUSE validation at71eab9f passed55 mount and eight
policy cases plus fallback, preserving existing mounts/checkout. Brick's older
kernel does not establish enabled passthrough. A missing /dev/fuse in the saved
Linux container is a failed prerequisite, never a passed real-mount test.

CI exercises Linux real mounts/performance/MSRV and native Mac behavior/bundle
compilation, both archives and corrected generated installers. Actual native
FSKit mount acceptance and exact final-head CI/Copilot review remain release gates.
