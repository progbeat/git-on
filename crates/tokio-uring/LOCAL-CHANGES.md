# Local descriptor ownership and completion cleanup

Based on crates.io tokio-uring 0.5.0, archive SHA-256
`748482e3e13584a34664a710168ad5068e8cb1d968aa4ffa887e83ca6dd27967`,
upstream commit `bf9906d052299dd05c3d13671800cd1632b1ee67`.
Original MIT license and upstream tests are retained.

Adds OpenOptions::open_at: the open operation owns a SharedFd for its directory
until kernel completion, including when the caller future is dropped. This
lets Git-on keep its existing directory-descriptor anchoring without resolving
mutable temporary parent pathnames or relying on /proc descriptor aliases.
File::from_shared_std retains an Arc<File> through native operations, sharing
the same descriptor with namespace metadata and passthrough without per-request
dup. File::try_into_std transfers a unique, idle owned descriptor back to its
standard owner; it refuses a shared descriptor or one with outstanding work.

Resource-producing open/accept operations close successful results abandoned
before consumption, whether already completed or still pending. Driver shutdown
keeps abandoned-operation data until the final completion, including buffer and
descriptor owners. Runtime construction keeps normal RAII until fallible driver
initialization succeeds, so an unavailable io_uring backend does not leak the
Tokio runtime's descriptors.

The upstream closed-FD test helper uses fcntl rather than constructing an owned
File for an invalid descriptor, which aborts under current Rust I/O safety.

The separate upstream.patch contains these local changes. No driver, executor
or syscall implementation is copied into Git-on.
