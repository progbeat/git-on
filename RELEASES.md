# 1.5.1
<!-- Bump: patch -->

- Reuse HTTP(S) Git connections for lazy reads from an interactive
  `git on` command.
- Combine pending file size requests on FUSE and NFS and use independent
  HTTP(S) connections to load several demanded objects concurrently.
