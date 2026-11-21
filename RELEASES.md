# 1.5.3
<!-- Bump: patch -->

- Serve recently read large Git blobs from RAM and spill them on eviction.
  Verify bytes read back from disk by block using BLAKE3. Retry a failed
  background spill when the next large blob enters the cache.
- Make verified blobs readable while their Git pack is still arriving in
  streaming HTTPS and SSH sessions.
- Use larger fetch batches when reads confirm a scan, including on fast
  HTTPS and SSH connections.
- Leave process file-size limits and their signal handling to the host.
  Git-on assumes unlimited `RLIMIT_FSIZE`. A finite limit can cause the
  operating system to terminate the run before cleanup.
- Create a private file on the first content change, using filesystem cloning
  when available. Private files use ordinary I/O, including Linux FUSE
  passthrough on eligible opens. Reads of immutable cached data keep their
  block authentication.
