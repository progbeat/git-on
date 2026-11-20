# 1.5.3
<!-- Bump: patch -->

- Serve recently read large Git blobs from RAM and spill them on eviction.
  Verify bytes read back from disk by block using BLAKE3.
- Make verified blobs readable while their Git pack is still arriving in
  streaming HTTPS and SSH sessions.
- When a tracked file first changes, verify the Git bytes carried into the
  private copy. Later reads and writes use ordinary file I/O, including Linux
  FUSE passthrough on eligible opens. The immutable cached source keeps its
  block authentication.
