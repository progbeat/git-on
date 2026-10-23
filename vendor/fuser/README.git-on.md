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
- `build.rs`, `mnt/mod.rs`, `mnt/fuse2_sys.rs`: macOS uses the existing libfuse2
  backend, loading the official `/usr/local/lib/libfuse.2.dylib` compat25 entry
  point with `dlopen`. Retain the library for process lifetime. This permits a
  native binary to start without macFUSE, for diagnostics and explicit-all
  materialization, and avoids installing a driver/SDK merely to build.

Linux remains on upstream's pure Rust backend with ABI 7.40. macOS keeps ABI
7.31 and macFUSE 4 compatibility. The inventory guard fingerprints the unchanged
trait and the full vendored source tree; regenerating/updating this dependency
requires explicit review. macFUSE mounting itself still needs real host
verification, with no permission to install/load extensions or alter security.
