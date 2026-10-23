# FSKit compatibility assessment for Git-on 1.3

FSKit is the preferred route to investigate on macOS 15.4+. No core Git-on
operation has been shown to require a kernel extension. The earlier VFS-only
prerequisite described the earlier raw-device implementation, not a proven
product requirement. No macFUSE installation or security change has been made.

## Concrete implementation gap

The earlier vendored fuser mount layer called `fuse_mount_compat25`, wrapped its
nonnegative result in `File`, and exchanged requests with `read`/`writev`.
FSKit has no such device descriptor. Modern macFUSE's macOS compat25 entry also
unconditionally returns -1; the new adapter replaces it. Official macFUSE's
[MFMount guide](https://github.com/macfuse/macfuse/wiki/Getting-Started-%28Developer%29-%E2%80%90-MFMount.framework)
provides the replacement: a message channel that retains the existing FUSE
parser, dispatcher and replies. Merely appending `backend=fskit` is insufficient.

The exact 5.3.3 sources are
[macFUSE's release tree](https://github.com/macfuse/macfuse/tree/macfuse-5.3.3),
[libfuse's mount selector](https://github.com/macfuse/library/blob/5c08befc7090bbe7f7c666dc2941aaba9e8537de/lib/mount_darwin.c),
[its message-channel implementation](https://github.com/macfuse/library/blob/5c08befc7090bbe7f7c666dc2941aaba9e8537de/lib/fuse_darwin_chan.c)
and the [public MFMount ABI](https://github.com/macfuse/mount/blob/313b9c68d04cd779bffc9f8bd9f32a4e1f5baf70/Mount/MFMount.h).
The adapter now implements channel/message ownership, complete multi-buffer
requests, replies, concurrent mount/INIT, bounded waiting and close/normal
unmount. It uses exported APIs, not private XPC messages. Local build, strict
clippy, fragment-copy/overflow tests and the actual Mac rename dispatcher
regression pass without a driver; these do not verify real mounts. The overlay
and Git backend are retained.

## Actual behavior to preserve

| Git-on behavior | Source assessment and FSKit verification |
| --- | --- |
| Raw committed bytes, lazy trees, symlinks, generated files | Ordinary lookup/read/write/create/link/rename callbacks. Keep Git/object fetching in the existing CLI daemon; the FSKit module only transports requests. Native FSKit mounts must test these operations, hard links, repeated readdir, compiled execution and mmap. |
| Four write policies | `Overlay::allowed`, `writable`, copy-up and namespace checks enforce these in userspace. Policy none additionally requests a read-only mount. Exercise every mutation against protected and generated entries; never relax a policy for backend compatibility. |
| POSIX permissions | `DefaultPermissions` currently supplies checks beyond explicit `access`/`setattr` handlers. Keep requesting it and verify chmod-000/0555 access, directory search, open/truncate and executable-bit failures. If FSKit does not enforce equivalent checks, add complete server-side checks or reject that backend; deleting the option alone is unsafe. |
| Owner-only access | The run parent is private mode 0700, and allow_other is never requested. Verify actual mount ownership and negative cross-user access. Private paths alone do not establish every backend's owner-only semantics. |
| nosuid and nodev | Keep requesting and inspect actual mount flags. Independently, overlay setattr masks permissions to 0777, mknod refuses special files, xattrs/file capabilities are unsupported and Git entries contain no device nodes. These defenses help, but do not establish accepted mount-option behavior without a real test. |
| Notification API | Git-on never calls fuser's notifier/invalidation API. The documented missing FSKit notifications are therefore not an existing requirement or a blocker. |
| Mount identity and cleanup | Verify whether fsname preserves the run's unique identity in getmntinfo. If FSKit represents it differently, persist and check an equally specific filesystem identity; never accept an arbitrary mount solely by path. Busy normal unmount must retain state; no forced/lazy production teardown. |
| Signals and caller identity | Whole-command signals/job control are OS process supervision. Per-request remote cancellation also depends on request identity; establish which uid/gid/pid the FSKit bridge supplies instead of assuming kernel-backend credentials. |
| macOS callback details | Check creation-time updates, rename flags, xattr refusal and statfs against the actual negotiated protocol. FSKit can expose callback gaps that basic reads do not reveal. Unsupported operations must fail without partial unintended mutation. |

The [backend limitations](https://github.com/macfuse/macfuse/wiki/FUSE-Backends)
do not specify exactly which of our mount options fail, so they are a test list,
not evidence that Git-on requires VFS. Apple's FSKit volume API includes ordinary
[file operations](https://developer.apple.com/documentation/fskit/fsvolume/operations)
and [access checks](https://developer.apple.com/documentation/fskit/fsvolume/accesscheckoperations).
The macFUSE FSKit module's implementation of every check is not established from
those Apple interfaces alone.

## Specific release/OS constraints

Read-only checks confirmed this Mac is 26.3.1(a), build 25D771280a. GitHub's latest
official macFUSE release is 5.4.0 (2026-09-07), not the planned 5.5.0.

In [macFUSE #1208](https://github.com/macfuse/macfuse/issues/1208), the maintainer
confirms unreliable read-only mount enforcement on older macOS. Planned 5.5
forwards the newer read-only API on 26.4+, whose reliability was reported fixed
in 26.5 beta 2. The adapter retains ro and rejects a missing actual read-only
flag for policy none. Userspace guards do not silently substitute for that flag.

In [macFUSE #1214](https://github.com/macfuse/macfuse/issues/1214), the maintainer
confirms FSKit lacks a per-file fsync callback. macFUSE 5.4 whole-volume sync
already sends fsync/fsyncdir for live handles; planned 5.5 adds flush before
release, while close cannot propagate its error on the discussed OS. Git-on
normally performs backing-file sync and propagates errors in these callbacks.
Whole-volume sync is not proven equivalent to each caller's fsync contract.
An actual callback/error/ordering probe is required. Synchronizing every write
is a possible conservative adaptation to investigate, but changes latency and
still needs kernel-cache ordering proof; it is not implemented or claimed here.
These are concrete remaining contract gates, not a blanket rejection of FSKit.
`tests/fsync_contract.py --binary target/debug/git-on` supplies that focused
probe for a `--features test-metrics` build: require a real callback for ordinary
fsync, then inject EIO in that callback and require the caller to receive it.
No callback, a false success or ignored error fails the probe. Its Linux real
FUSE execution is part of CI; it has not been executed through FSKit here.

## Test and installation boundary

To validate the channel adapter, use a macOS 15.4+ Apple Silicon host or
VM with an owner-provided official macFUSE package and enabled FSKit file system
extension. Explicitly select FSKit; report the selected backend. This route does
not require Recovery's kernel-extension security reduction. Package/helper
installation and enabling the user-space extension still need the owner's
approval. MFMount may try to install/update helpers during mounting, so confirm
the provisioned helper state first; quiet mode only controls its dialogs.

Run `tests/macos_mounts.py` with the actual installed distribution, add the
permission/ownership/mount-flag/identity probes above, and port busy recovery and
terminal stop/continue scenarios. Repeat mounts and remote interruption must
work. The same path is a candidate for a hosted macOS 15/26 test job once package
installation is authorized. Neither successful compilation nor a generic VM
kernel-extension limitation answers whether FSKit works.

FSKit's module isolation is not a sandbox for the launched command. The command
retains the user's ordinary host access, as before; no new sandbox guarantee,
entitlement, signing credential or elevated child is part of this adaptation.

## Direct Apple FSKit alternative

A native Swift/Objective-C FSKit extension could share Rust snapshot, policy and
overlay logic while Linux keeps FUSE. It would remove macFUSE as a runtime
dependency. However, Overlay currently implements fuser callbacks and reply
types, so sharing it directly requires a backend-neutral operation/result layer
and an Apple API bridge; it is not a different mount option for this adapter.

Distribution would also need a containing app/extension bundle, an installer or
installation flow, signing/notarization using owner-provided credentials, and
user activation of that extension. Git/SSH execution and host access must remain
in an ordinary CLI/helper or be mediated with supported resource/IPC permissions;
embedding Rust does not grant an extension unrestricted host access. These are
additional packaging and sandbox decisions beyond a single command-line binary.
The shared Apple FSKit per-file sync limitation also needs resolution; replacing
macFUSE alone does not establish that contract. No direct-FSKit rewrite, signing
credential creation or extension installation is authorized or implemented here.
