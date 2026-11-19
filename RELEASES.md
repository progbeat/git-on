# 1.5.1
<!-- Bump: patch -->

- Reuse one HTTP(S) Git connection for lazy reads from an interactive
  `git on` command.
- Combine already pending file metadata requests on FUSE and NFS to reduce
  Git round trips during concurrent reads of a remote snapshot.
