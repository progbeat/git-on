# 1.5.3
<!-- Bump: patch -->

- Use direct Git protocol v2 for network loads. HTTPS requires HTTP/2 and
  SSH requires OpenSSH. Select `HEAD`, a full SHA-1 OID, a full ref, or an
  unambiguous branch or tag. Use a direct URL or configured remote and a
  full ref if names compete. Older HTTP/Git transports, proxies, and native
  network fetch fallbacks have been removed. Local paths and `file://` remain
  supported.
- Read explicit local sources, including `file://` and named remotes, with
  the same revision semantics as `git -C PATH on REV`. Use `HEAD:subdir` to
  select a local subtree. Local revisions no longer write fetch destinations.
  Native local object reads require Git with `--no-lazy-fetch` (Git 2.45+).
  Complex alternates use the source reader without a temporary bare repository.
  `Snapshot::source_git_dir()` reports the local Git directory or `None` for a network snapshot.
- Remove system `git fetch` from preparation and lazy reads, along with
  private fetch repositories, staged pack/index files, and packet trace replay.
- Restore the legacy Rust `Snapshot::load_with_pin_record` signature.
  Both pin-record loaders accept a git2 0.20 `Oid` callback. The loaders
  ignore these callbacks and their return values. Use git2 0.21 OIDs for
  `Snapshot::open` and `read`. A caller holding an older OID can recreate
  it with git2 0.21’s `Oid::from_bytes(old.as_bytes())`.
- Document when OpenSSH can run configured `Match exec` commands, including
  during configuration inspection before connecting.
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
  buffers. Use an existing writable tmpfs or the system temporary directory
  for the private files required by native verification and index resolution.
- Use Git's configured signature verifier in its normal environment. A signed
  annotated tag selects the tree recorded by the verified tag object even
  when the tag ref moves. For supported remote copy runs, verify the streamed
  commit or tag with Git without staging a pack or index.
- Decode one shallow snapshot pack in memory for HTTPS and SSH copy runs.
  Preserve the selected OID when automatic mounting falls back to copy.
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
