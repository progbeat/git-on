# Dependency reuse audit for milestone 1.4.0

Audited master: `27cd1e5c336541566f9497186425c44e4aa46f73`.
Decisions precede the resource-path changes; this document records the code/API
inspection rather than a general dependency-upgrade campaign.

| Function | Decision | Code, API and tradeoff |
| --- | --- | --- |
| CLI | Retain clap Builder and boundary parser | `src/cli.rs` already shares clap help/schema. The first `--`, literal child argv and byte-exact option-looking/non-UTF8 operands require the current boundary handling. Derive adds no needed behavior. Existing CLI/native Git oracle tests remain the gate. |
| Memory cache | Adopt lru 0.16.3, default features off | Replaces the paired BTreeMap indexes in `snapshot::Cache` with expected O(1) lookup/touch/eviction instead of O(log K). Uses std HashMap, adding no hashbrown dependency; retains the wrapper's byte and entry budgets, oversized bypass and Arc/Weak reader lifetimes. MSRV 1.70, MIT. |
| Temporary progress | Adopt indicatif 0.18.3, default features off | One ASCII spinner, delayed 500 ms, stderr TTY only, disabled for TERM=dumb. RAII clears and joins before child launch/error return. console, portable-atomic and unit-prefix are its native dependencies; no renderer, Unicode-width or async runtime is required. MSRV 1.71, MIT. |
| Ratatui | Reject for this scope | Inline rendering exists, but no application UI, layout, input handling or full-screen state is required. |
| Global persistent cache | Remove | The confirmed scope keeps only private per-invocation Git object storage. Removing cross-run persistence removes its file index, flock, serialization, fsync, retention and crash-recovery code altogether. Historical directories are untouched; no replacement daemon is introduced. |
| rusqlite | Reject for this scope | There is no persistent cache to index. A database would introduce unnecessary native build/schema and maintenance cost. |
| Moka / memory LRU for disk | Reject for this scope | Cross-run disk persistence is removed; retain the existing bounded decoded memory cache instead. |
| Git object decoding | Retain git2 0.20.4 / vendored libgit2 1.9.7 | Odb::reader streams loose decoded bytes (compressed source is mmap-backed); the pack backend lacks readstream and may require whole-object/delta decoding. Use that API before falling back to odb.read; do not claim packed streaming. Native Git unpack-objects imports just-received remote packs into loose run storage, avoiding cumulative pack discovery; its big-file threshold streams large non-delta blobs. |
| Hash verification | Adopt RustCrypto sha1 0.10 | Computes the canonical Git type/size/payload digest incrementally before publishing staged data. It shares the existing digest/cpufeatures/cfg-if dependency closure with sha2; no new crypto/native runtime. MIT OR Apache-2.0, verified by the Rust 1.89 locked-graph gate. SHA-1 identifies Git's existing object format; this does not add SHA-256 repository support. |
| Pack descriptors | Retain libgit2 resource policy | opts::set_mwindow_file_limit already supplies LRU closure of inactive pack windows. Set once before threads, capped at 32 and a quarter of effective RLIMIT_NOFILE; do not raise the OS limit. Temporary Repository/ODB/object handles remain scoped to operations. |
| Temporary files / signals | Retain tempfile and signal-hook | Existing unlink-on-drop spools, atomic persists and cancellation ownership remain applicable. |
| clap_complete | Defer | Completion is optional new UX outside this milestone. |

API/maintenance evidence is the pinned crate source and Cargo manifests in the
locked graph, including libgit2's `odb_loose.c`, `odb_pack.c` and `mwindow.c`.
The dependency lock and target-specific notices identify the distributed closure.
MSRV, platform builds, license generation and observable behavior are validated
by the repository's existing Rust, macOS and native packaging checks. Measurements
and red/green regressions belong to the resource acceptance evidence, separately
from these adoption decisions.
