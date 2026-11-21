# 1.5.3
<!-- Bump: patch -->

- Serve recently read large Git blobs from RAM and spill them on eviction.
  Verify bytes read back from disk by block using BLAKE3. Retry a failed
  background spill when the next large blob enters the cache.
- Make verified blobs readable while their Git pack is still arriving in
  streaming HTTPS and SSH sessions.
- Decode full objects and deltas from reusable HTTPS and SSH sessions in
  memory from the first fetch. Stream native decoder requests and borrow cached
  blob ranges when serving reads.
- Use larger fetch batches when reads confirm a scan, including on fast
  HTTPS and SSH connections.
- Leave inherited process resource limits to the host. Git-on assumes the
  working set fits in available RAM.
- Read local repositories without creating temporary references in them.
- Create a private file on the first content change, using filesystem cloning
  when available. Private files use ordinary I/O, including Linux FUSE
  passthrough on eligible opens. Reads of immutable cached data keep their
  block authentication.
