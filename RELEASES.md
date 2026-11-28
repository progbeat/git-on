# 1.5.3
<!-- Bump: patch -->

- Run local snapshots without creating refs in the source repository. Git-on
  reports a read error if Git garbage collection removes an unread object.
- Recover stale run directories without changing source refs. Runs that recorded
  a ref in the source repository still need 1.5.2 for stale cleanup.
- Resolve explicit local paths and local remotes with the same revision rules
  as a repository selected with `git -C`.
- Stream GitHub-compatible Git v2 packs over direct HTTPS/HTTP/2 and OpenSSH.
  Named remotes follow the same path as direct URLs. Native verification uses
  the exact commit or annotated tag with the user's configured verifier.
- Retain recently used large blobs in memory and write cache payloads only
  on eviction.
