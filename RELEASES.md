# 1.5.3
<!-- Bump: patch -->

- Serve recently read large Git blobs from RAM and spill them on eviction.
  Verify bytes read back from disk by block using BLAKE3.
- Make verified blobs readable while their Git pack is still arriving in
  streaming HTTPS and SSH sessions.
- Create a private file on the first content change, using filesystem cloning
  when available. Private files use ordinary I/O, including Linux FUSE
  passthrough on eligible opens. Reads of immutable cached data keep their
  block authentication.
