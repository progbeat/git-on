# 1.5.1
<!-- Bump: patch -->

- Reuse HTTP(S) Git connections for lazy reads from an interactive
  `git on` command.
- Overlap exact directory-tree and file-size requests on FUSE and NFS over
  independent HTTP(S) connections.
- Expand the HTTPS connection pool on slower sources and batch overlapping
  requests for objects the command has already accessed.
- Allow more independent requests on NFS mounts with parallel HTTP(S) reads
  within the existing per-session replay memory limit.
- Let short NFS attribute and lookup calls proceed while another slot waits
  for a Git object, including calls under a different UID in the same session.
- Multiplex Git object fetches on one HTTP/2 connection where the HTTPS
  source and caller configuration support it.
- Reuse authenticated channels during startup for exact object IDs and
  unambiguous remote branches.
- Preserve native Git selection for force-prefixed fetch refspecs.
- Wake promptly when a native Git subprocess exits on macOS, reducing polling
  delays during snapshot preparation and pack decoding.
- Open spare SSH channels while the initial one negotiates Git and during
  slow root fetches. Let ready channels serve queued reads during setup.
- Prefetch upcoming blobs from already opened directories on both transports.
  Grow the fetch batch during scans that consistently use the prefetched blobs.
- Decode Git packs as they arrive and make verified objects readable before
  the rest of the pack finishes downloading.
