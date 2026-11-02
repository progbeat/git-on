# Directory-relative native file opens

Based on crates.io tokio-uring 0.5.0, archive SHA-256
`748482e3e13584a34664a710168ad5068e8cb1d968aa4ffa887e83ca6dd27967`,
upstream commit `bf9906d052299dd05c3d13671800cd1632b1ee67`.
Original MIT license and upstream tests are retained.

Adds OpenOptions::open_at: the open operation owns a SharedFd for its directory
until kernel completion, including when the caller future is dropped. This
lets Git-on keep its existing directory-descriptor anchoring without resolving
mutable temporary parent pathnames or relying on /proc descriptor aliases.
No driver, executor or syscall implementation is copied into Git-on.
