# Dependencies and implementation boundaries

| Responsibility | Implementation | Constraints |
| --- | --- | --- |
| Linux FUSE | Registry `fuser` 0.18 | The adapter converts typed callbacks and owns backing handles. Kernel capability negotiation controls passthrough and caching; supported operations retain their fallback behavior. |
| Git storage and native configuration | `git2` with vendored libgit2, and native Git children | Git interprets revisions, sources and refspecs. Credentials, proxy/TLS and signature policy use native configuration. Packed/delta decoding can require complete decoded objects. |
| Canonical object verification | `sha1-checked`, plus the audited native SHA1DC ODB path | Verify Git type, length and payload before publication; reject detected collisions. Only typed verified ODB objects may reuse the native proof. SHA-1 identifies the supported Git object format. |
| Temporary-storage authentication | `sha2` | Ordered SHA-256 block expectations and exact lengths stay in protected RAM. Authenticate the actual reloaded buffer before returning bytes. |
| Payload retention | `lru` and standard collections | One byte-accounted 16 MiB LRU, one retained entry per original OID. Metadata and active decoder/read allocations are accounted separately. |
| Directory metadata | Standard maps, shared immutable structures and scoped merged views | Parse demanded trees once, support direct name lookup and stable active enumeration versions, and retain only issued inode references. |
| Pack inflation | `flate2` with zlib | Incremental decoding requires stream completion, exact framing and canonical OID verification. Necessary delta dependencies retain native handling. |
| Temporary files | `tempfile` and anchored no-follow file operations | Invocation ownership, inherited file/descriptor limits, failure cleanup and same-UID tamper protection apply to every storage path. |
| Cancellation and children | `signal-hook` and scoped native process ownership | Signals and shutdown preserve owned-child cleanup and documented recovery behavior. |
| Remote sessions | Native Git upload-pack, HTTP helper and SSH transport with bounded protocol framing | Reuse invocation-owned sessions and prepared objects. Unsupported protocols or native policy settings use the compatibility path. |

The exact dependency versions and sources are recorded in `Cargo.lock`.
Native verification assumptions are tied to that locked build. Changes to the
Git/hash dependency closure require checking those assumptions again.

[Resource limits and integrity invariants](resources.md) apply across these
boundaries. Rust 1.89 checks, platform builds, license notices and observable
behavior are validated by the repository's required test and packaging jobs.
