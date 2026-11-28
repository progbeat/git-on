# Dependencies and implementation boundaries

| Responsibility | Implementation | Constraints |
| --- | --- | --- |
| Linux FUSE | Registry `fuser` 0.18 | The adapter converts typed callbacks and owns backing handles. Kernel capability negotiation controls passthrough and caching; supported operations retain their fallback behavior. |
| Git storage and native configuration | `git2` with vendored libgit2, and native Git children | Native Git resolves local revisions and reads its configuration. libgit2 reads local objects, with native `cat-file` for object storage it cannot read. Git handles credential lookup and signature verification. |
| Canonical object verification | `sha1-checked`, plus the audited native SHA1DC ODB path | Verify Git type, length and payload before publication; reject detected collisions. Only typed verified ODB objects may reuse the native proof. SHA-1 identifies the supported Git object format. |
| Immutable spill verification | `blake3` | Ordered BLAKE3 block digests and exact lengths stay in memory. Each disk block is checked before its bytes are returned. |
| Payload and ignore-rule retention | `lru` | Mounts keep 16 MiB of small payloads and spill blocks, plus eight whole large blobs. Eager copy uses a separate 128 MiB payload cache. Compiled ignore rules have a 4 MiB / 128-entry cache. |
| Directory metadata | Standard maps, shared immutable structures and scoped merged views | Parse demanded trees once, support direct name lookup and stable active enumeration versions, and retain only issued inode references. |
| Pack inflation | `flate2` with zlib | The shared Git v2 decoder inflates entries and applies deltas in memory. It checks object identity before publication and checks the complete pack on transfer completion. |
| Temporary files | `tempfile` and anchored no-follow file operations | Invocation ownership, inherited file/descriptor limits, failure cleanup and same-UID tamper protection apply to every storage path. |
| Cancellation and children | `signal-hook` and scoped native process ownership | Signals and shutdown preserve owned-child cleanup and documented recovery behavior. |
| Remote sessions | libcurl HTTP/2 and OpenSSH | Remote object requests use direct Git v2. The transports reuse invocation-owned connections and verified objects. |

The exact dependency versions and sources are recorded in `Cargo.lock`.
Native verification assumptions are tied to that locked build. Changes to the
Git/hash dependency closure require checking those assumptions again.

[Resource limits and integrity invariants](resources.md) apply across these
boundaries. Rust 1.89 checks, platform builds, license notices and observable
behavior are validated by the repository's required test and packaging jobs.
