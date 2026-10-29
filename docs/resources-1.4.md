# Resource acceptance for milestone 1.4.0

The implementation is compared with master
`27cd1e5c336541566f9497186425c44e4aa46f73`. The PR's exact-head Actions runs and
JSONL artifacts are the final acceptance evidence. The early local measurements
below describe the development candidate; they are not a substitute for final
Linux/FUSE/macOS validation or a numerical SLA.

## Cache complexity and crash policy

Let K be retained cache occupancy, N the number of global cache operations, E
the total actual evictions, B the total transferred payload bytes, and R the
number of dirty recoveries. File/directory lookup costs of the host filesystem,
Git's storage decoding and network work are separate parameters.

The old implementation collected metadata for all K payloads and sorted on
every restore/store, with a second scan/sort in store: O(K log K) maintenance per
operation, or O(sum K_i log K_i) over N operations. Bounded capacity is not a
reason to treat K as constant.

The new filesystem index has a fixed-size header and one bounded record per
OID, with previous/next links. Direct OID lookup, a touch and insertion each
read/write a constant number of records. Normal aggregate metadata work is
O(N + E), with O(K) persistent metadata and O(1) index RAM. Payload transfer and
hashing add O(B), plus required Git decoding; a restore deliberately makes two
streaming passes before publication. Eviction counts each removed object. Run
ODB hits bypass the global cache entirely.

Initialization and recovery from an interrupted mutation enumerate and sort the
published payloads once, costing O(K log K) time and O(K) temporary metadata RAM.
Total work including recovery is O(N + E + sum over R of K_r log K_r + B), plus
Git/filesystem costs. Clean lock acquisition never schedules a periodic scan.
There is no promise of amortized constant work under an adversary that kills
every writer: repeated real interruptions can cause repeated recovery scans.
No child or run-local disk quota is introduced.

Writers hold flock from recovery through publication/eviction. A durable dirty
marker precedes mutation. Node/header writes use tempfile atomic rename and
fsync; affected directories are synced before removing the marker, then its
removal is synced. Recovery trusts only atomically published payload paths,
rebuilds links and capacity accounting, and discards abandoned staging files.
Payload hashes/types are checked before restore and again before ODB finalize.
Atomic staging can temporarily consume one additional candidate record; it is
not retained capacity. Index metadata is separately bounded in K. The old v1
namespace remains untouched, preventing an older binary from encountering v2
index files after rollback.

The permanent Rust cases vary K=8/64/256 and N=16/128 independently; they assert
bounded metadata operations with zero ordinary recovery scans. They cover hits,
misses, byte/entry eviction, recency, corrupt payloads, retained active copies,
concurrent independent processes and a SIGKILL between link update and commit.
The latter must recover exactly once and reclaim staging files. A working set
larger than retention is allowed misses on later runs; it must not re-fetch an
object still available within an active run.

## Focused old/new measurements

Local executor: Linux x86_64, kernel 6.18.55, glibc 2.41, five visible Intel Xeon
Platinum 8573C CPUs; Git 2.52.0, Rust 1.99 debug builds, temporary overlay-backed
storage. OS page caches are uncontrolled. The source probe exercises production
loading without a FUSE mount. Baseline binaries came from a separate exact-master
worktree and target directory, never a relabeled current binary.

`tests/remote.py` with `--before-resource-probe` reproduces old failures and
requires corrected behavior:

- At soft/hard RLIMIT_NOFILE=64, old cold loading failed at approximately the
  54th blob with `start native Git operation: Too many open files`. New cold
  loading read all 192 distinct blobs, sampling 19 descriptors after 16, 64,
  128 and 192 objects. A warm run sampled four at each point. No ulimit was raised.
- First 16-byte ranges from compressible loose blobs, followed by repeated
  ranges, gave the following Linux VmHWM observations. Each new run performed
  one streamed spool write, retained zero blob-cache payload bytes and reused
  that spool. Compressed-source mmap/page cache is distinct from the eliminated
  full decoded application buffer.

| Object size | Old peak RSS (KiB) | New peak RSS (KiB) |
| --- | ---: | ---: |
| 16 MiB | 20,224 | 4,016 |
| 64 MiB | 69,520 | 4,232 |
| 128 MiB | 135,356 | 4,392 |

This is evidence for loose streaming, not a packed/delta streaming claim.
Packed first reads may still allocate the decoded object and necessary bases.
The 288 MiB over-capacity remote test separately verifies cold transfers,
eviction, warm cache-to-ODB/source-spool streaming, exact bytes, and same-run
availability after global eviction.

`tests/cache_scaling.py` traces only the loader process, excluding native Git
children. It seeds K records and initializes the index outside measurement,
then varies demanded N independently. All measured payloads are hits.

| K | N reads | Old cache metadata syscalls | New cache metadata syscalls |
| ---: | ---: | ---: | ---: |
| 64 | 8 | 737 | 31 |
| 64 | 32 | 2,345 | 103 |
| 256 | 8 | 2,849 | 31 |
| 256 | 32 | 9,065 | 103 |

Old cache-directory getdents calls were 22/70/22/70; new calls were zero.
The JSONL also records wall time, flock-held intervals and internal maintenance
counts separately. Under strace, the new atomic/durability path took longer in
these small-cache samples (roughly 0.8–2.6 s held, versus 0.2–0.5 s old); ptrace
amplifies its extra publication syscalls. This is not claimed as a wall-time win.
The structural result is independent of K, without hiding durability, payload
hashing or actual eviction costs. Untraced and final CI timings remain separate.

## Full repository-scale matrix

`tests/scalability.py` builds increasing 1,000, 10,000 and 1,000,000-leaf fixtures
and reads every leaf through an actual command. The full-tree fixture has
six one-digit components at one million leaves, ten-way directories, one shared
eight-byte leaf blob, one `.gitignore` control blob and six distinct tree objects
plus a signed commit: nine distinct objects and 8,000,005 logical bytes. Leaves
are 100644 and directories 040000. Reuse is explicit: this controls namespace,
complete traversal/read and materialization growth. It does not replace the
existing million-distinct-seven-byte-blob lazy fixture and its exact fetch gates.
Neither fixture is a maximum supported repository size or proof at every size.

For each source/policy/backend cell, both verified and unverified signed commits
run no-op, sparse stat/read with 100 repetitions, and complete traversal/read.
Remote runs explicitly start cold and repeat warm. Eager materialization also
runs an inert child and necessarily visits the entire selected tree before
launch. Each workload checks bytes, path completeness, startup/result, exact
source refs and cleanup. Native git clone plus the same three commands establishes
the technical-failure baseline; direct execution time on that disk copy is
recorded separately as the performance baseline. Clone preparation is excluded
from command timing. A per-invocation harness timeout is not a product SLA.

| Backend | Sources | Policies | Host/evidence |
| --- | --- | --- | --- |
| Ordinary FUSE | Local and real HTTPS remote | none, ignored, untracked, all | Required Linux `repository-scale` matrix |
| Generated-file passthrough | Local and real HTTPS remote | ignored (generated I/O path) | Existing isolated capability test copy in `real-fuse`; ordinary child identity retained |
| Explicit-all materialization | Local and real HTTPS remote | all | Linux and Apple Silicon macOS `repository-scale` matrix |
| Native macOS mount | Inapplicable | Inapplicable | Deferred #12, never counted as a pass |
| Protected materialization | Inapplicable | none, ignored, untracked | Must fail before child, existing materialized tests |

JSONL records exact HEAD, fixture OIDs, OS/CPU/limits, cache/backend, child-start
and total times, wait4 peak resident memory, sampled Linux runner descriptors/RSS,
sampled filesystem space change, demanded objects, requests/bytes and cache
maintenance. Filesystem space change is host-wide; wait4 includes waited
subprocesses and is not a simultaneous sum; Mac runner-only /proc measurements
are unavailable and represented as such. Unsupported/skipped platforms are not
passes. Linux real-FUSE, Mac materialization and packaged-binary tests retain
their distinct evidence layers. Final readiness requires the exact-head matrix,
existing callback guard/integration/stress suites and Copilot review, not only
a loader pass or a sparse million-file sample.
