# Reviewed fuser 0.16.0 compatibility patch

Source: crates.io fuser 0.16.0, checksum
`0bb29a3ae32279fe3e79a958fe01899f5fb23eadccee919cf88e145b54ed9367`.
The upstream MIT license is retained. The Filesystem trait is unchanged.

Local changes:

- `ll/request.rs`: parse legacy 16-byte INIT and advertised 64-byte extended INIT
  separately, clamp the reply minor version and reflect the client extension bit.
  This fixes upstream ABI7.40 short-INIT incompatibility without changing the
  Filesystem trait. Parser unit and actual protocol-dispatch regressions cover it.

- `reply.rs`: add the two ReplyCreate passthrough methods corresponding to
  ReplyOpen. This fills the upstream 0.16 create-reply gap, using the existing
  `Response::new_create` backing ID field. No new ioctl implementation.
- `build.rs`, `channel.rs`, `session.rs`, `mnt/macfuse.rs`: macOS uses the official
  MFMount framework's message transport and explicitly selects FSKit. The old
  libfuse compat25 function returns -1 on macOS in modern macFUSE and cannot
  supply a device descriptor for FSKit. Load the fixed official framework path
  at runtime, retain it for process lifetime, copy complete message buffers
  before releasing them and wait for concurrent mount/INIT before launch.
  No driver/SDK is needed merely to build or use explicit-all materialization.
  MFMount may refresh its own helpers; they must be owner-provisioned first.
- `ll/request.rs`, `request.rs`: preserve macFUSE FUSE_RENAME flags, translating
  exclusive/swap high bits to the portable callback flag values. An actual Mac
  protocol-dispatch regression reproduces the previous silently lost flag.

Linux remains on upstream's pure Rust backend with ABI 7.40. macOS keeps ABI
7.31 and the macFUSE protocol layout. The inventory guard fingerprints the unchanged
trait and the full vendored source tree; regenerating/updating this dependency
requires explicit review. macFUSE mounting itself still needs real host
verification, with no permission to install/load extensions or alter security.
