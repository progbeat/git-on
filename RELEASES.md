# 1.5.1
<!-- Bump: patch -->

- Reuse HTTP(S) Git connections for lazy reads from an interactive
  `git on` command.
- Overlap exact directory-tree and file-size requests on FUSE and NFS over
  independent HTTP(S) connections.
- Expand the HTTPS connection pool on slower sources and batch overlapping
  requests for objects the command has already accessed.
