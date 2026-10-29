# Resource acceptance for milestone 1.4.0

The implementation is compared with master
`27cd1e5c336541566f9497186425c44e4aa46f73`. The PR's exact-head Actions runs and
JSONL artifacts are the final acceptance evidence; results from earlier heads
with a global cache do not establish acceptance after its removal. Early local
measurements below describe the development candidate; they are not a substitute for final
Linux/FUSE/macOS validation or a numerical SLA.

## Object storage and eliminated global-cache work

The author confirmed that temporary storage for one invocation must remain and
the global persistent cache must be removed. The interview's preference for
streaming reads/writes does not require cross-run persistence. The prior global
cache's O(K log K) scans and the intermediate linked index's constant metadata
operations both imposed filesystem, serialization, locking and durability costs.
The final design removes the global cache entirely instead of adding a database
or service. No claim about O(1) makes those constant costs free.

Each remote invocation fetches missing demanded objects into its private ODB.
Already available objects bypass native fetch and remain available after memory
cache eviction. A later invocation repeats native resolution and fetches its
own demands; it does not reuse another run's objects. Global-cache occupancy K,
evictions and lock/fsync work are absent from this architecture, rather than
hidden constants. Historical user cache directories are neither opened nor
deleted. Private run storage has no artificial disk quota; system limits apply.
Decoded memory caches retain explicit byte and entry budgets.

Permanent regressions run without HOME/XDG_CACHE_HOME, with a cache path that is
a regular file, and with marked historical v1/v2 directories. All must succeed
without modifying those paths. Independent HTTPS invocations must each request
the demanded objects; repeated access within one invocation must not fetch them
again. Object integrity, native errors and cancellation remain required.

## Focused old/new measurements

Local executor: Linux x86_64, kernel 6.18.55, glibc 2.41, five visible Intel Xeon
Platinum 8573C CPUs; Git 2.52.0, Rust 1.99 debug builds, temporary overlay-backed
storage. OS page caches are uncontrolled. The source probe exercises production
loading without a FUSE mount. Baseline binaries came from a separate exact-master
worktree and target directory, never a relabeled current binary.

`tests/remote.py` with `--before-resource-probe` reproduces old failures and
requires corrected behavior:

- At soft/hard RLIMIT_NOFILE=64, old cold loading failed at approximately the
  54th blob with `start native Git operation: Too many open files`. An early
  candidate read all 192 blobs with 19 descriptors; the final
  private-loose-ODB path samples three after 16, 64, 128 and 192 objects on
  each independent invocation, and reuses an evicted memory entry without
  fetch. No ulimit was raised.
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
The remote fixture now verifies streaming three 16 MiB blobs (48 MiB total),
above both the 8 MiB memory-cache and native Git import streaming thresholds,
exact range bytes, same-run availability and a fresh invocation's repeated fetch.
The earlier 288 MiB control exceeded a global-cache quota that no longer exists;
local 16/64/128 MiB first-range scaling remains unchanged.
There is no global retention/restore path to benchmark.

## Distinct-object demand and pack discovery

Independent review of `d0edf505bfbf5b2280d63fa60a68791b24911996` identified another
parameter: P, the cumulative number of per-demand packs. The long-lived ODB
refreshed after every demand, and each remote ensure opened another Repository.
libgit2 pack refresh enumerates/sorts all packs, including on already available
run objects. A low descriptor count alone did not fix this metadata work.

Remote fetches now import only newly received pack(s) into the private run's
loose ODB using native `git unpack-objects`, then discard those pack/index
files. All companion indexes, including optional `.rev`, leave the ODB before
unpacking so the input cannot be mistaken for an already retained copy.
`core.bigFileThreshold=8m` makes native Git stream large non-delta blobs during
import; delta decoding remains Git's necessary work. Existing object filtering,
explicit OID requests, cancellation and integrity checks remain in force. This
is not a startup preload or a request for unrelated objects. Operation-local
Git handles close before return. Snapshot demand reuses its existing Repository
and no longer unconditionally refreshes the ODB.

The run retains zero per-demand packs between operations, while loose objects
remain available until run cleanup. Pack discovery sees only the current
fetch's pack(s), independently of earlier demands. This adds one native unpack
operation and recompression of received bytes, accounted for separately from
payload retention; it avoids cumulative pack enumeration/sorting. Over N demands,
pack maintenance is linear in the actually received packs/objects, plus necessary
Git decoding and bytes. No cap is imposed on active run storage or repository size.

`tests/pack_scaling.py` loads 32, 128 and 512 distinct blobs twice in one fresh
private ODB and checks same-run availability. Its loader-only strace separates pack
metadata from native Git children. The source probe's OID
reporting enumerated run objects after every read in the historical samples
below. The current metrics-only mode verifies the available OID set once after
both passes, removing quadratic test-helper work. Wall time is not presented
as isolated product throughput.

| Distinct blobs | d0edf50 pack-directory bytes enumerated | Corrected bytes |
| ---: | ---: | ---: |
| 32 | 2,559,376 | 22,784 |
| 128 | 37,495,312 | 88,064 |
| 512 | 742,274,064 | 349,184 |

The permanent reduced-RLIMIT test also inspects the live private pack directory:
it must be empty at increasing object counts. Its observed independent-invocation
descriptor samples were three, rather than the initial cache-bearing candidate's
19/four. The separate local packed-reader limit still protects libgit2 accesses
to callers' packed repositories. These distinct-demand controls supplement both
million-file fixtures; they are not a claim of testing a million distinct remote
payload demands in a full traversal.

## Pre-merge repository-scale coverage

The historical `2062eb8` acceptance run completed 378 invocations, including
42 full million-leaf reads across fourteen physical scale jobs and two enabled
passthrough groups. Its full-tree fixture reused nine Git objects and one shared
eight-byte payload; the separate million-distinct-file lazy fixture remained
independent. Those results are historical evidence, not a claim about the new
default CI workload.

The author subsequently authorized consolidating repeated checks and varying
stress parameters, while keeping important validation before merge. The revised
[CI cost and coverage map](ci-performance.md) identifies each distinct invariant,
measured old costs and the replacement execution plan. Every previous
source/policy/signature combination still runs on representative 1k/10k trees
when the relevant behavior, dependencies, shared helpers or workflows change.
Unknown changes require all heavy suites; base functional checks always run.
Selected signed 100k full reads cover local/remote ordinary FUSE, Linux/Mac
explicit-all materialization and actual generated-file passthrough. These full
children check startup, complete names, varied bytes, stat and repeated reads.

A separate HTTPS/FUSE control traverses **one million leaf names**, verifies
all directory/name sets, and samples one payload per leaf directory. It is not
a full million-payload read or a million-file materialization. The existing
million genuinely distinct seven-byte-blob lazy fixture, no-op/read fetch gates,
unused-subtree controls, large ranges, reduced-RLIMIT tests and increasing
32/128/512 distinct-object demands remain before merge. Neither a stress count
nor a test timeout is a product support ceiling or SLA.

The wide namespace fixture has up to sixteen eight-byte payloads and two
alternating subtree variants. At one million paths it has 23 distinct Git
objects and 8,000,005 logical payload bytes, with 100-way three-component paths.
At 100k it has 21 objects. Object reuse is explicit; it does not model a million
distinct payload demands. Full byte-reading pressure is now limited to the
selected stress sizes, rather than claimed equivalent to the old 42 million
payload opens. The permanent real-4s-unmount-helper regression and real busy/
escaped-reader/signal tests retain their independent failure triggers.

Each source uses an immutable Git fixture and read-only direct baseline. Actual
invocations get fresh private snapshots/ODBs; generated output must be absent
at child entry. Native clone plus the **same child command** remains the
technical-failure control. Direct command time is separate from clone/setup.
New records explicitly time fixture generation, clone, clone deletion, startup,
child phases, time after child and final cleanup. Sample-file allocated blocks
are observed per filesystem, never assumed universally. Filesystem deltas are
host-wide; wait4 includes waited subprocesses rather than a simultaneous sum.
Mac runner-only /proc observations are unavailable and labeled as such; denied
Linux /proc and filesystem observations remain independent.

Native Mac mounts remain deferred to #12. Protected materialization remains
inapplicable and must fail before child execution. Native Linux/Mac package,
protocol/callback guard, full functional/transport/signature suites and seeded
filesystem differential stress remain. The existing required `package` gate
validates the conservative change plan, all base jobs and actual artifacts for
every required scale/stress/lazy/resource case. Required skipped or missing
coverage fails the gate. A manual exhaustive matrix is available before release;
no important case is intentionally deferred until after merge. Final readiness still requires exact-head CI,
a fresh independent review and final Copilot review.
