# 1.4.0
<!-- Bump: minor -->

- Add streaming transfers for loose Git objects, large source spools and persistent-cache restores. Packed objects still use libgit2's required object/delta decoding. Objects too large for the persistent cache are rejected from their headers, without decoding them just to decide retention.
- Bound retained pack descriptors using the effective process descriptor limit, leaving room for active files and native Git startup. This fixes cold remote loading that could fail with “Too many open files”; git-on never raises your OS limits.
- Replace repeated full-cache scans with a persistent LRU index. The 256 MiB payload and 4096-object retention limits remain; bounded index metadata is separate. Already demanded objects stay available in each run despite global eviction.
- Successful explicit-all materialization is quiet when output is redirected. Lengthy terminal preparation shows a temporary spinner that clears before the command starts, including error and interruption cleanup.
- The indexed cache uses `git-on/objects-v2` under XDG_CACHE_HOME (or HOME/.cache). The first run after upgrading starts cold. The older `objects-v1` cache is left untouched so older git-on binaries can still use it; it can be removed when no longer needed.
- CLI argument bytes, write policies, object verification and lazy FUSE behavior remain unchanged. macOS continues to support explicit-all materialization; native macOS mounts remain deferred. Repository/file counts have no artificial ceiling; the million-file scenarios are acceptance fixtures, not a support maximum.
