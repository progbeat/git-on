# Direct Apple FSKit backend

macOS uses our own FSKit file-system app extension in GitOn.app. It does not
load macFUSE, MFMount, libfuse or a kernel extension. Linux uses the published
fuser 0.18.0 crate, without a local fork.

The shared Rust Overlay owns pinned Git objects, lazy lookup, policy decisions,
namespace changes, byte I/O, metadata and handles. Linux's typed FUSE callbacks
are a thin adapter. Optional kernel backing registrations stay in that Linux
adapter. The native FSKit extension translates Apple's operations over a private
per-run native-operation channel to the same engine. It does not implement the
FUSE wire protocol or run Git/SSH inside the extension sandbox.

The private resource directory is owner-only (0700), its Unix socket is0600,
and both ends verify the connecting UID. The extension receives a public
FSPathURLResource, uses its security scope and holds it through volume lifetime.
The Rust daemon retains normal user Git/SSH access; no extra privilege is given
to the launched command. This remains a disposable filesystem, not a host sandbox.

## Public API and build requirements

The extension uses FSUnaryFileSystem, FSVolume Operations/ReadWrite/OpenClose/
AccessCheck and requestedMountOptions. The last API supplies read-only mounting
from macOS26.4, which is the bundle deployment target. CLI startup and explicit-all
materialization retain macOS13+. Apple SDK26.5 compiled and linked the local
Objective-C/Swift adapter on macOS27.0.1.

The [official Apple sample](https://developer.apple.com/documentation/fskit/building-a-passthrough-file-system)
uses a containing app and FSKit app extension, sandbox entitlement and
com.apple.developer.fskit.fsmodule. Our extension additionally declares network
client access for its local Unix socket. `scripts/build_macos_bundle.py` creates
the unsigned bundle and explicit entitlement file; it never installs, signs,
registers or activates it. No kernel-extension security reduction is required.
Distribution signing/notarization and actual local activation still require
owner-approved existing resources. No credentials are created by the build.

## Acceptance still required

Compiling an extension is not native mount acceptance. Before support is claimed,
run the packaged binary/bundle through all four policies; bytes, links/modes,
compiled execution/mmap, mutation/metadata, cross-user isolation, actual
nosuid/nodev/read-only flags, exact getmntinfo identity, signals/job control,
busy recovery and checked normal unmount/cleanup. Native permission and rename
behavior must be tested on the actual OS; no protection may be dropped to make
a mount pass. Linux protocol peers do not prove these Apple kernel behaviors.

FSKit does not expose a per-file fsync callback. Volume synchronization is not
claimed equivalent to per-file durability/error propagation. Persistent databases
belong outside the disposable tree; crash-durable temporary databases are not
an added release requirement. Correct ordinary I/O and policy/lifecycle behavior
remain required. The strict Linux fsync callback/error probe remains separate.
