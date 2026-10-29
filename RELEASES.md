# 1.4.0
<!-- Bump: minor -->

- Stream loose Git objects into verified large source spools without an avoidable full decoded RAM buffer. Local packed objects still use libgit2's required object/delta decoding.
- Bound retained pack descriptors using the effective process descriptor limit, leaving room for active files and native Git startup. This fixes cold remote loading that could fail with “Too many open files”; git-on never raises your OS limits.
- Import received remote packs into loose run storage and reuse the run Repository. Available objects avoid repeated ODB refreshes, and later demands no longer scan all earlier per-object packs. Native Git streams large non-delta imports; necessary delta decoding remains.
- Remove the global persistent object cache and its disk maintenance. Remote objects stay in the private temporary repository for the invocation and are reused within that run; separate invocations fetch their own demanded objects again. Decoded memory caches remain bounded.
- Successful explicit-all materialization is quiet when output is redirected. Lengthy terminal preparation shows a temporary spinner that clears before the command starts, including error and interruption cleanup.
- Terminal progress pauses while native Git owns the foreground, including shells using `stty tostop`. Clean unmount waits for active inode reclamation rather than failing large successful traversals after three seconds; completed busy-mount failures still preserve recovery state.
- Historical `git-on/objects-v1` and `objects-v2` cache directories under XDG_CACHE_HOME or HOME/.cache are left untouched. Git-on no longer requires those environment variables for object caching and starts no cache daemon.
- CLI argument bytes, write policies, object verification and lazy FUSE behavior remain unchanged. macOS continues to support explicit-all materialization; native macOS mounts remain deferred. Repository/file counts have no artificial ceiling; the million-file scenarios are acceptance fixtures, not a support maximum.
