# 1.5.3
<!-- Bump: patch -->

- Resolve named SSH refs through native Git when a selection channel
  advertises `ref-in-want` so a lagging replica can serve the tag by name.
- Fetch each tree individually on a lazy remote mount so filtered Git
  responses include every requested subdirectory and file.
- For slow sequential reads, check SSH channel completion while the pack
  is arriving when another channel is ready. After the command exits, start
  closing SSH input while used packs finish validation.
  When mount startup falls back to copy, release expired or unneeded SSH
  channels before opening another channel on the owned connection.
- Serve recently read large Git blobs from RAM and spill them on eviction.
  Verify bytes read back from disk by block using BLAKE3. Retry a failed
  background spill when the next large blob enters the cache. Check for
  cancellation between spill writes.
- Group private run directories under `TMPDIR/git-on` and remove the empty
  shared directory after the last run cleans up. Derive stale mount identity
  from the run directory and keep native remote snapshots without Git pin refs.
  Use the private group when a regular file already occupies `TMPDIR/git-on`.
  Use a writable ownership marker for run and recovery locks on NFS.
  New runs without a persistent directory identity require manual recovery.
  Deferred NFS cleanup preserves the command's exit status. Reclaim run
  files if space runs out while parking a legacy cleanup marker.
- Make verified blobs readable while their Git pack is still arriving in
  streaming HTTPS and SSH sessions.
- Decode full objects and deltas from reusable HTTPS and SSH sessions in
  memory from the first fetch. Stream native decoder requests and borrow cached
  blob ranges when serving reads.
- Keep direct reusable HTTPS and SSH startup metadata in verified in-memory
  buffers. Store the files required by native Git in a private directory on
  an existing writable tmpfs or in the system temporary directory. Scan the
  native ref advertisement trace with bounded memory.
- Use Git's configured signature verifier in its normal environment. A signed
  annotated tag selects the tree recorded by the verified tag object even
  when the tag ref moves. For supported remote copy runs, verify the streamed
  commit or tag with Git without staging a pack or index.
- Stream the first filtered fetch from compatible local paths and `file://`
  URLs without staging a pack or index.
- Decode the shallow snapshot pack in memory for compatible HTTPS, SSH, and
  local file transport copy runs selected by a full Git OID, an unambiguous
  branch or tag, or `HEAD`.
  HTTPS copy can stream over HTTP/1.1 as well as HTTP/2 and can query refs before
  retrying a refused first fetch by full OID. Preserve the selected OID when
  HTTPS switches to native Git or mounting falls back to copy.
- Wake the startup indicator immediately when preparation finishes.
- Skip the extra HTTPS ref query when mounting a full Git OID.
- Let Linux FUSE reuse negative lookups of names in a read-only snapshot.
- Send NFS reads from existing buffers when those allocations fit the session's
  replay budget.
- Use larger fetch batches when reads confirm a scan, including on fast
  HTTPS and SSH connections.
- Leave inherited process resource limits to the host. Git-on assumes the
  working set fits in available RAM.
- Read local repositories without creating temporary references in them.
  Reuse a native Git object reader for alternate stores that need it.
  If external Git maintenance removes an unread object, the operation that
  needs it reports a source read error.
- Disable Git hooks and filesystem monitors during preparation. Keep stored
  HTTPS credentials and legacy source refs unchanged. A configured remote
  using the default branch mapping can stream without a temporary Git repository.
- Create a private file on the first content change, using filesystem cloning
  when available. Private files use ordinary I/O, including Linux FUSE
  passthrough on eligible opens. Reads of immutable cached data keep their
  block authentication.
