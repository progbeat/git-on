# Resource acceptance for milestone 1.4.0

## Current simple store (implementation work in progress)

The approved scale reference is WebKit; the absolute clone-success guarantee
was replaced in the interview. No claim of completed WebKit acceptance follows
from that wording. The current implementation stores object descriptors and
packed ordered SHA-256 block hashes in trusted RAM, after full canonical Git
OID verification of the same stream. A byte-accounted 16 MiB data LRU has no
independent total-object cap, but retains only one entry per original OID across
all readers. Objects up to 64 KiB stay whole in RAM until eviction; large objects
stream to disk and retain at most their most recently read block. A new block
replaces the old cache entry without invalidating owned buffers of active reads.
A sequential large scan therefore cannot occupy the LRU with many blocks from
one object. Backing contents use one raw file per original object. Disk reads verify full intersecting 64 KiB
blocks before returning bytes from that checked buffer. Resident rereads avoid
those disk hashes. No disk object/name index or HMAC key remains.

Touched unique directories are parsed once into shared immutable RAM structures;
raw tree bytes are released. Parsed directories, verified descriptors and touched
header sizes grow with demand outside the payload LRU. Report this metadata and
actual process memory separately: the LRU limit is not a total-RSS guarantee.
Local packed/delta decoding retains the documented libgit2 memory limitation.
Libgit2 can also retain decoded delta bases (up to 1 MiB each, 16 MiB per
pack); anonymous retention is not limited to active decoder buffers.
Source-pack mappings are a separate
cache: startup selects 8 MiB windows and a 16 MiB soft mapped-byte budget, as
well as the existing descriptor cap. This replaces libgit2's 64-bit defaults of
1 GiB windows / 8 GiB mapped bytes. Active windows and necessary delta decoding
can exceed those retained-cache budgets; they are not object-size or RSS limits.
A 192 MiB non-delta source-pack experiment, after all 48 readers closed, retained
192 MiB of source-pack RSS under the defaults versus 8 MiB with these options;
process RSS was 206.3 versus 22.3 MiB and the decoded LRU retained 3.0 MiB in
both. This isolates clean source mappings, not heap buffering or physical
memory pressure. The permanent 48 MiB packed regression verifies bounded idle
mappings and exact first/last bytes without decoding objects again.
Protocol packs and native decoder stdout can still need transient bounded-part
staging; that I/O and all native children remain inside total-cost measurements.
No global cache, recovery fallback or corrupt-spill retry is introduced.

The evidence below refers to the explicitly named historical implementations.
In particular, old disk-index/HMAC/4 KiB-tag descriptions are history, not the
current design. New first/repeat, real-FUSE and WebKit measurements are required
before claiming that the current code satisfies the full overhead contract.

## Fixed setup versus demand work

`tests/startup_profile.py` uses a fixed 160-distinct-blob source, a minimal
`true` command and the same Python command reading 0/1/16/64/160 files. Each
Python command performs first and closed-repeat passes in one invocation.
Three shuffled trials per local/remote source use fresh invocation storage and
fresh complete native clones; source OS caches are warm/uncontrolled. Total
clone + identical command + cleanup remains the baseline. Fixed setup is never
subtracted from acceptance, nor labelled unavoidable merely because it appears
in a no-op run. All waited subprocess CPU and baseline cleanup CPU are recorded.

Only `test-metrics` binaries recognize the private timing descriptor. Fifteen
monotonic events separate source resolution/preparation, backend setup, command
spawn/run, unmount, transport/raw-store cleanup and filesystem/pin cleanup. The
full parent-observed time also includes program load/CLI parsing and diagnostic
serialization outside those events. The private channel is close-on-exec;
child stdout/stderr and exit status remain ordinary. Missing/reordered events
or partial cleanup fail the diagnostic; actual FUSE is required. These phases
and size curves diagnose costs, not a numerical SLA or an asymptotic proof.

## Performance acceptance remains in progress

Issue #20 remains mandatory for 1.4.0. The c42ebf1 run's 100k local/remote
FUSE full-read child times (22.927s/37.700s versus 1.052s/1.397s directly from
disk) do not establish acceptable overhead. Passing resource limits and a
linear pack-metadata gate does not establish this requirement either. No
numerical allowance is inferred from the interview.

A bounded 32/128-unique-object profile included native Git descendants. For
128 seeded 8 KiB blobs it observed 130 unpack processes, 1,208,006 bytes of
pack/index file writes followed by 1,054,489 bytes of loose-object writes and
390 object-file sync calls. These are logical syscall bytes, not assumed SSD
write volume. Untraced total time was 4.755s, with 2.973s waited user+system CPU;
the debug probe also serializes its results, so this is attribution evidence,
not production CLI throughput. Traced timings include substantial observer cost.

The initial independent kernel-cache change gives positive lookup/data cache
lifetimes only to policy-protected pinned source. Directory attributes remain
uncached because generated children change timestamps/link counts. Mutable
entries and `all` keep the previous behavior. The wire regression checks these
boundaries. `tests/io_profile.py` uses real FUSE to compare two complete 1k walks
with that policy enabled/disabled in the same instrumented executable, plus the
identical command directly from disk. It captures waited CPU including descendants and counts actual delivered
lookup/getattr/open/read callbacks in the test-only metrics. The FUSE runs are
untraced: ptrace suppresses the setuid mount helper's privileges. Native Git
file writes/syncs are measured separately with descendant-following tracing. The private comparison knob exists only in test-metrics
builds. This initial change is not claimed to complete #20.

Real FUSE at 3bc4dc0 (CI 41032542008) measured two complete 1k walks at
812ms uncached versus 413ms cached, with an identical direct-disk command at
55ms. Delivered lookup/read/getattr calls fell from 4846/2202/18163 to
1015/1001/7954; open calls remained 2202. First/repeated walk times were
380/366ms uncached, 198/150ms cached, 20/17ms direct. This is a real improvement,
not acceptable-overhead or full-#20 evidence. All normal CI jobs passed.

Private native Git destination commands now disable durability barriers, while
caller repositories, verification/trust settings and hash-before-trust remain
unchanged. Interrupted invocation data is not resumed after a machine crash;
stale cleanup only removes it. An inherited core.fsync=all (and the obsolete
core.fsyncObjectFiles switch) must not add syncs to disposable object bytes.
A full-process trace regression observed 16 object syncs before and zero after.
The 128-object profile changed from 390 to zero sync calls, but its untraced
wall time increased from 4.755s to 5.245s amid other local work. No wall-time
speedup is claimed from that single noisy comparison; loose recompression and
writes remain.

The initial large-blob increment retained verified raw bytes after close/reopen.
An append-only unlinked payload file and an unlinked, randomized, linear-probing
index retain two descriptors (three during geometric index growth), constant
index RAM and O(K) disk for K distinct prepared large blobs. Expected amortized
index lookup/insert is O(1); rehash work is counted, not hidden as a constant.
Payload transfer and canonical OID verification remain proportional to decoded
bytes, using a 64KiB buffer. This replaces one active-only spool per blob; no
named global cache or daemon is introduced. New retained-raw metrics distinguish
run-retained disk bytes from active readers and bounded decoded RAM caches.

A Linux debug profile read three individual bytes from each local loose blob,
closing after every read. For 16MiB/64MiB objects, the old path prepared
48MiB/192MiB (three spools), the new path 16MiB/64MiB (one). Total times changed
0.702/2.193s to 0.269/0.747s; new repeated one-byte reads took 0.45–0.91ms.
These observations include waited-child CPU, have uncontrolled page caches and
are not optimized CLI throughput. Three application payload bytes still caused
64MiB of first verification/transfer. This diagnostic does not count metadata
interface bytes and does not establish the complete N-based bound. The initial
preparation is now compared against the author-clarified full-revision
materialization baseline. The repeated
64MiB preparation is eliminated, rather than excused by amortization.

The subsequent raw-import increment removes blob recompression and duplicate
loose writes, retaining verified bytes only within one run (details below).
The author clarified the baseline as full materialization of the same revision
followed by the identical direct command: T_git_on <= T_materialize + T_direct
+ O(N). N includes command payload bytes and metadata API results, including stat, in
a fixed interface representation. All metadata operations are included in total
cost; no internal work or native Git subprocess is excluded. Required initial fetch/decode/verification
may use the materialization budget; already prepared data must not be fetched
or unpacked again, including close/reopen and memory-cache eviction. The
large-blob change alone did not address small-object eviction or remote
pack-to-loose recompression and was not completion of #20.

## Raw-import increment and remaining work

Native `cat-file --batch --buffer` writes newly received objects directly to the
invocation's raw arena. Canonical OID verification then reads those bytes in
bounded chunks. Blobs are published in place; only commit/tree/tag metadata is
written into the native ODB needed for resolution/signature verification. The
received packs are removed before the next demand. Local blobs also retain
verified raw bytes, including small blobs after decoded-memory-cache eviction.
The 140-distinct-blob regression leaves no loose blob copies or retained packs
and does not fetch or decode the first blob again on its second closed read.

Prepared raw disk bytes must not become a new trust boundary. At the fca7786
checkpoint, three unlinked files retained payload, block tags and disk index,
with a fourth descriptor only during geometric index growth. The subsequent
per-file-limit correction below changes their backing, retaining authentication.
Initial SHA-1 verification simultaneously writes
32-byte HMAC-SHA256 tags for 4 KiB logical blocks. A random invocation-only key
and fresh preparation nonce bind tags to OID, size, generation and block number.
Index records are authenticated too. Later ranges authenticate exactly the
returned bytes, avoiding whole-blob rehashing and a second unchecked read. This
adds bounded tag I/O and CPU, not another payload copy; these costs remain in
the production measurements. The assumed protection of process memory is the
existing interview model, not a new system/security-setting requirement.

Open readers hold weak cache references and fall back to authenticated raw
storage after eviction; they do not retain every small payload outside the RAM
budget. Their registry uses expected O(1) hashing and removes closed readers on
drop instead of scanning all live readers on each open. The permanent tests
reproduce both the extra retained RAM and stale registry entry at `325be51`.
Linux native-operation waiting now uses optional pidfd exit notification, with
the existing polling fallback on unsupported kernels or descriptor pressure.
Cancellation and job-control polling retain their previous maximum interval.

`tests/import_profile.py` exercises the optimized production CLI with distinct
seeded 8 KiB blobs, two complete closed read/stat passes, and a fresh run store.
The comparison is a full `file://` clone, the identical command, and clone
cleanup. Wall time and waited descendant CPU include the entire runner and its
cleanup; an included `wait4` observer records largest waited-process RSS (not a
sum of concurrent RSS). Native file writes are traced in a separate small
materialized run, including every Git child. Traced wall time is not used as a
speed benchmark. The fixed native stat ABI size is recorded; internal bytes are
never added to application N. CI requires exact-head clean evidence from real
FUSE and materialized runs, and rejects missing native-write observations.

Three local untraced trials with no concurrent build/test workload used 160
distinct 8 KiB blobs (1,310,720 source payload bytes). The authentic optimized
`325be51` binary took 5.432/5.568/6.092s; the raw-import candidate took
3.143/3.163/3.290s, a 1.76x median wall-time improvement. Waited user+system CPU
medians were 3.461s and 3.339s: CPU cost barely improved. Full clone + identical
command + cleanup baselines were 0.163/0.175/0.209s. These uncontrolled warm-page
cache observations clearly leave a large gap; they do not establish acceptable
overhead. The current full-process peak observation was 8448 KiB (largest waited
process, not simultaneous sum). The two application passes transferred
2,621,440 payload bytes and 46,080 explicit metadata result bytes on this
144-byte native stat ABI. Internal copies are not added to N.

A separate 32-blob trace recorded 34 native decoders and no `unpack-objects`.
Decoder output wrote 265,204 bytes directly to raw storage; index-pack wrote
303,938 pack/index bytes and the main native ODB received only 970 metadata
bytes. All other fixture writes, including backing copies, raw tags/index and
cleanup-related files, were observed too (319,706 bytes in the other-process
category plus 2,192 fetch-output bytes). These are logical successful syscall
write bytes, not physical device traffic. The retained-pack metadata control at
32 demands enumerated 16,880 pack-directory bytes in 428 metadata syscalls; it
remains a partial complexity check, separate from the complete CLI profile.

This increment does not complete #20. Remaining costs include per-object native
fetch/import processes, native metadata re-decoding after tree-cache eviction,
FUSE metadata traffic and writable copy-up/materialization copies. Local packed
decoding constraints remain. Smaller representative production profiles do not
replace exact-head million-file lazy and supported-platform checks, and neither
finite counters nor partial pack traces prove the total asymptotic contract.

## Per-file limits and eager request batching

Independent review of fca7786 reproduced an introduced P2: with
RLIMIT_FSIZE=64 KiB, 80 distinct 8 KiB local files failed at the ninth file because
all small blobs now shared one aggregate raw file. Exact parent 33548b3 and an
ordinary native clone succeeded. The permanent materialized regression also
reproduces the remote variant with compressible distinct blobs, where a native
file:// clone fits the same limit. Both are checked with NOFILE=64 as well.

Raw payload, tags and index now use bounded physical parts under the private
invocation directory, outside the child view. Part size is min(1 GiB, inherited
FSIZE); it is not a total-storage or repository-size ceiling. Arithmetic offsets
avoid a retained map of all part names. One cached descriptor per logical file
and one shared directory descriptor remain constant; index rehash adds one.
No-follow anchored opens reject non-regular, foreign or hardlinked parts.
HMACs still verify the actual returned bytes. Native stdout uses one bounded
pipe/writer thread and writes each payload to disk once, without an object-size
prepass. Output failure stops the owned native operation; cancellation/TTY work
stays on the requesting thread. This introduces bounded pipe copying, explicitly
included in total CPU/time, while eliminating an aggregate file-size constraint.

Eager materialization can batch known required entries; lazy FUSE cannot infer
future requests. Groups of at most 256 IDs from the current prepared directory
share native fetch/import, skip already available IDs and deduplicate each
batch. The bounded 302-path/301-blob regression crosses a batch boundary and
reuses a shared first/last blob: 303 explicit object demands require four native
fetch/decode operations, and each unique blob is prepared once. Complete byte
checks and the reduced-FSIZE local/remote regression cover the result. Native
operation counters and independent process tracing remain distinct from
object-demand counts.

Three paired local production-CLI trials on the final pipe/part candidate used
160 distinct 8 KiB blobs and two closed read/stat passes. fca7786 total times
were 6.138/4.492/4.429s, versus 0.599/0.463/0.506s: medians 4.492s and 0.506s
(8.88x). Waited user+system CPU medians were 4.634s and 0.779s (5.95x). Full
clone + identical command + cleanup baselines were 0.903/0.411/0.292s, median
0.411s. Host/page-cache variability is explicit; neither the earlier direct-file
prototype nor its more favorable timings is substituted for this final pipe
path. This materially improves eager preparation but is not #20 acceptance or
a new allowed slowdown. The lazy per-object native-process path remains.

The separate 32-blob trace observes three fetches, three native decoders and
zero unpack operations. Native output is now IPC; the Rust staging worker writes
the raw parts. All descendant file I/O is retained: 273,396 raw-part write bytes,
268,103 index-pack bytes, 971 native metadata bytes, 295,145 other runner/backing
write bytes, 921 fetch-file bytes and 1,530 upload-pack-file bytes. IPC bytes are
not mislabeled as disk writes or added to application N. CI compares native
operation counters to the actual trace and requires the staging disk writes.

A bounded metadata diagnostic confirms the next unresolved cost independently
of those payload improvements. Ten different stat lookups in a 1k-entry directory
parse its tree once (55,016 cached bytes). The same ten lookups in a 100k-entry
directory parse the tree ten times and retain zero tree-cache bytes because its
decoded metadata exceeds the 4 MiB budget. Individual debug-loader observations
were 75–118ms each for the wide directory. This is not a complete invocation or
baseline benchmark: it reproduces the repeated full-directory decode/sort path,
which costs O(Q * R log R) for Q lookups in an oversized R-entry directory. It
must be replaced with invocation-local prepared metadata lookup before #20 can
be claimed. Diagnostic source/results are retained at
`/tmp/git-on-140-tree-redecode.py` and `/tmp/git-on-140-tree-redecode.jsonl`.

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

The run retains zero per-demand packs between operations, while metadata and
raw blobs remain available until run cleanup. Pack discovery sees only the
current fetch's pack(s), independently of earlier demands. The first fix used
one native unpack operation and recompression; the raw-import increment replaces
it with one native decoder and no blob recompression. Over D distinct demands,
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

### Invocation-local directory preparation increment

The former bounded tree cache re-decoded an oversized 100k-name tree for each
lookup, and decoded evicted small trees again. `oversized_directory_metadata_is_prepared_once_for_repeated_lookups`
and `metadata_eviction_keeps_prepared_directory_entries_available` reproduced
21 versus 1 and 4 versus 2 source-tree loads respectively before the increment.
Prepared raw trees and sealed name tables now survive memory eviction. A name
lookup uses a bounded memory map or expected O(1) disk probes plus name bytes;
enumeration still costs the entries returned. Directory-handle/frontend memory
and unrelated namespace operations are not covered by that narrow bound.

The permanent `tests/metadata_profile.py` measures optimized CLI preparation,
two closed stat/read passes and cleanup against a complete native clone plus
the identical child and cleanup. Its wide tree shares one one-byte blob across
1k/100k names: three reachable Git objects, explicitly **not** a distinct-object
or full-payload traversal fixture. The separate import profile retains that
dimension. N counts each command read and a fixed native stat ABI result; all
descendant CPU remains included in total costs. Real FUSE execution requires
an actual device. Source-cache state is declared warm/uncontrolled rather than
called cold. Final per-head CI records are required; this increment alone does
not establish the complete issue #20 acceptance contract.

An initial local release diagnostic at `9a1d9e7`, before eliminating redundant
materializer lookups, measured one invocation per size on the same generated
source/filesystem. Total materialization, identical command and cleanup were:

| Flat names | Native full baseline | Git-on explicit-all | Git-on waited CPU |
| ---: | ---: | ---: | ---: |
| 1,000 | 0.152 s | 0.071 s | 0.071 s |
| 10,000 | 0.473 s | 0.311 s | 0.310 s |
| 100,000 | 5.325 s | 3.944 s | 3.920 s |

Each command performed 1,000 stat/read/close operations per pass. An independent
loader diagnostic on the 100k tree took 1.090 s for startup, ten stat lookups and
cleanup, with one decode and 100k prepared entries. The first preparation cost
is included; warmed lookups alone are not the benchmark. These single trials
have source/OS cache and ordering noise and provide no general performance SLA.
Evidence and scripts are retained outside /tmp in
`/workspace/git-on-evidence/metadata-029872d/`. The existing lazy remote FUSE gap,
including per-object native process startup, remains unresolved by these numbers.

### Historical synthetic CI plans, before the WebKit replacement

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
a fresh independent review. The owner cancelled further Copilot reviews.

### WebKit replacement and directory-attribute stage

The synthetic plan above is historical. The current affected plan keeps all
24 behavior interactions at 1k paths, removes repeated 10k/selected 100k cases
and the shared-object million-path namespace job, and requires a separate
WebKit job. The exact pin, baseline composition, process/transfer metrics,
remaining controls and measurement limitations are in
[the current CI map](ci-performance.md). Adding the job is not evidence that
WebKit has run or that the whole overhead requirement has been met.

The useful demo runs 32 upstream `webkitcorepy` unit tests (version, string
utilities and filtered calls), using the exact command in [the test guide](../tests/README.md).
Direct `unittest` runs the same tests registered by `test-webkitpy` without that
entrypoint's unconditional bulk `AutoInstall.install_everything()`. Python `-S`,
`-B` and the supported autoinstaller disable flag avoid package setup and bytecode
writes; no browser build or SDK is needed. A selective pinned-source preflight
passed all 32 tests: three first/repeat trials had median whole-process times of
288 / 253 ms and a largest waited-process peak near 21 MiB. This is a local
bootstrap check, not a full-WebKit/FUSE performance claim. The earlier 39-module
AST workload remains only a diagnostic, superseded as the useful demo.

The first full pinned WebKit CI workload completed both passes with exact names,
modes and bytes. At `e1edf15`, whole local Git-on time was 457.48 s versus a
141.94 s composed materialize + command + cleanup baseline; first/repeat reads
were 228.59 / 184.52 s versus direct 15.57 / 14.27 s. Unique nonempty blobs
loaded once (411,092); this is not acceptance of the remaining overhead. That
run's aggregate failed because partial `/proc` visibility was rejected by its
memory observer. Follow-up evidence uses a primary `wait4` largest waited-process
peak, while retaining explicitly partial process-tree estimates; no complete
aggregate physical-memory measurement is claimed.

Linux now returns positive attribute TTLs for protected source directories.
Their metadata was already immutable under the existing overlay rules; this
removes redundant callbacks without caching generated membership. The new
protocol regression preserves generated-name visibility and zero mutable/all
TTLs. Actual-kernel gain and coherence remain CI evidence, not a protocol-only
claim. Inclusive callback/transport spans are gathered separately from ordinary
timing trials so profiling instrumentation is not presented as uninstrumented
latency. Session compressed pack payload bytes and native stored received-pack
bytes are distinct counters; index-pack normalization and wire framing prevent
treating the latter as an exact on-wire byte count.

### Measured WebKit unit-test demo at 9010d4c

[CI 41183217607](https://github.com/progbeat/git-on/actions/runs/41183217607)
passed the pinned full-WebKit case and all six fresh local/file:// unit-test
invocations. Every first/repeat pass ran 32 real upstream tests successfully.
Use the [exact command](../tests/README.md) on the pinned revision; the runner
executes it twice with fresh Python processes in one mounting. No SDK, browser
build, package installation or bytecode writes are needed.

Three-trial medians, optimized binary, warm/uncontrolled source OS caches:

| Measurement | Local Git-on | file:// Git-on | Already materialized direct |
| --- | ---: | ---: | ---: |
| Whole invocation, two test processes and cleanup | 415 ms | 599 ms | 347 / 350 ms |
| First test process | 182 ms | 324 ms | 148 / 150 ms |
| Repeated test process in the same mount | 164 ms | 167 ms | 149 / 149 ms |
| Sampled process-tree RSS sum | 60.6 MiB | 68.7 MiB | 47.9 / 48.2 MiB |

Local/remote composed complete-materialization baselines were 114.10 / 113.58 s;
one measured preparation and cleanup sample per source was reused explicitly.
Native complete clones retained 1,851,088,455 received pack bytes. The small
command demanded only 35 unique blobs / 293,353 verified payload bytes and ten
trees. Remote invocation received 103,382 session-pack payload bytes plus 1,234
native stored received-pack bytes; these exclude framing and public-network
costs. No raw payload was spilled while these fitting objects remained resident.
The largest waited-process peaks were about 27.8 MiB for both Git-on and direct
commands. The RSS sums above count shared pages repeatedly and are non-atomic;
their difference is a diagnostic estimate, not additional physical peak memory.

The same head's complete two-pass namespace/payload workload took 495.88 s
against a 143.96 s complete baseline. First/repeat passes were 235.16 / 214.76 s
versus direct 15.80 / 14.35 s. All 411,092 nonempty distinct blobs decoded once,
but significant repeat overhead remains. Its largest waited-process peak was
788 MiB, with sampled tree RSS 813 MiB. The previous head's larger RSS was a
partial estimate; cross-run timings are not an isolated causal speed comparison.
This useful small-command result does not complete #20 or justify the full
traversal cost. The subsequent independent review also found missing final
failure propagation for caught raw-storage EIO; its fix requires fresh head CI.

### Full-path diagnosis and backing-cache advice

The labelled `082e5f6` diagnostic on a 4-vCPU AMD EPYC / 16 GiB CI VM took
322.88 s against a 139.32 s complete baseline. This is not an uninstrumented
speed comparison with f272504: direct command passes also fell from 16.02/14.08 s
to 10.91/9.01 s on this host. Command time was 262.73 s; owned state cleanup
(including raw files) took 58.56 s. Inclusive raw disk-read spans totalled
55.05 s, requester checks 30.74 s and lookup callbacks 16.71 s; nested spans
must not be added as independent costs. The invocation incurred 5.69 GB of
waited block input and decoded every nonempty unique blob only once.

All sampled direct-copy pages remained resident. The FUSE view retained fewer
sampled pages while prepared raw copies also occupied cache; the invocation's
file cache approached 13.7 GB. A subsequent Linux NOREUSE experiment requested
lower backing-page retention without discarding data or initiating explicit
writeback. Its syscall regression passed, but the real workload did not show a
reduction in raw disk-read work or physical input. The hint and its advice-only
test were therefore removed from the final code. DONTNEED was not used because
Linux's implementation may initiate asynchronous writeback.

CI at `ea351f1` passed all 14 applicable jobs. The full diagnostic took 460.35 s
against a 163.71 s complete baseline on a different 4-vCPU EPYC 9V74 host;
its 177.52/208.49 s command passes compare with direct 14.07/11.78 s. Cross-host
elapsed changes cannot establish causality. Raw disk reads still requested
9.446 GB in 544,495 calls (previously 9.442 GB / 544,430), with 5.602 GB physical
input (previously 5.688 GB). Inclusive requester checks fell from 30.74 s to
27.98 s despite higher pack-decode/lookup times on this host; the paired local
syscall observation below isolates allocation reuse more reliably. All source
decoding, subprocess work and cleanup remain included; #20 is not complete.

Requester checks continue reading the current task status on every poll, but
reuse a thread-local scratch allocation instead of repeatedly growing an empty
buffer. A paired syscall observation of the existing real-disposition test
reduced status reads from 125 to 65 while retaining all 30 seeks/polls and
passing the same signal cases. Large supplementary-group lists still read to
EOF. This is a focused syscall result, not evidence that the full workload's
30.74 s requester-check span has disappeared.

### Readonly open/release round trips

For `--write-policy=none`, the Linux adapter now negotiates the existing FUSE
NO_OPEN_SUPPORT capability when the peer offers it. An initial ENOSYS open
response lets Linux use fh=0 and omit subsequent open/release requests; one
ENOSYS flush response similarly disables pointless close-flush requests for
this immutable connection. Metadata, range reads and explicit sync still have
their ordinary callbacks. Reads use the same Snapshot and verified owned-buffer
path, including for objects larger than the data budget. No generated file or
mutable policy uses this mode. Older peers keep ordinary handles.

The permanent protocol regression fails before negotiation is implemented and
checks fallback policies, type/mode errors, fh=0 attributes and ranges, explicit
sync, bad handles, forget and subsequent lookup. A larger-than-budget unit case
checks one decode, one retained cache entry and no handle retention. The mounted
regression additionally exercises independent opens, dup, mmap, fstat and sync
on a 17 MiB file; it requires an actual FUSE host. This removes a known source of
protocol work, but its effect on the complete WebKit workload must be measured.
