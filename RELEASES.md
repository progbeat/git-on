# 1.5.0
<!-- Bump: minor -->

- Run commands on lazy Git snapshots on macOS 26 and later with any of the four
  write policies. Git-on serves the snapshot over a private Unix socket to the
  built-in NFSv4.1 client. The mount needs no elevated privileges or extensions.
- macOS 13 through 15 continue to support explicit `--write-policy=all`
  through a temporary copy. Auto can use the same fallback when the NFS mount
  is unavailable and git-on successfully cleans up any partial mount.
- Select a backend with `--backend=auto|fuse|nfs|copy`. Explicit `fuse` and
  `nfs` require a mount. Explicit `copy` requires `--write-policy=all`.
