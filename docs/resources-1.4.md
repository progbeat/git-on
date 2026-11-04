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
one object. Payloads that need temporary backing use one raw file per original
object. Disk reads verify full intersecting 64 KiB blocks before returning bytes
from that checked buffer. Resident rereads avoid those disk hashes. No disk
object/name index or HMAC key remains.

Local Linux FUSE objects up to 64 KiB remain whole in RAM while resident. Before
eviction releases those verified bytes, the store saves an uncompressed private
file using the existing asynchronous I/O path. A later miss reloads that file
and compares the returned buffer with the original trusted block hash before
cache admission. It does not reread or decode the original Git object. RAM hits
neither perform file I/O nor rehash. Large local objects still stream to raw
storage. Remote objects keep their invocation-owned verified storage and never
refetch because of decoded-cache eviction. The eviction write must complete
before releasing the resident entry; background spill submission is not yet
implemented.

The `blob_loads` snapshot metric counts verified demanded-object descriptors,
not native decoder calls. This counter alone does not establish absence of
repeated work. Permanent local-source regressions additionally close the source,
remove its original Git object and verify exact bytes after eviction; source
demand counts stay unchanged across disk and RAM rereads.

Touched unique directories are parsed once into shared immutable RAM structures;
raw tree bytes are released. Source leaves retain their original OID, not a full
source path; readonly leaf lookup also omits the ordered/reverse pathname indexes
used by mutable namespaces. Directory paths remain available for descendant
resolution. Parsed directories, verified descriptors and touched
header sizes grow with demand outside the payload LRU. The wholly readonly
`none` policy also permits kernel directory-page caching on FUSE ABI 7.28+;
mutable policies retain live directory I/O. This reuses only enumerated pages
and does not fetch leaf contents or visit unrequested subtrees. Report metadata and
actual process memory separately: the LRU limit is not a total-RSS guarantee.
When the kernel advertises NO_OPENDIR_SUPPORT, readonly cacheable directories
omit per-open/release RPCs and use the kernel's zero handle. Source preparation
remains lazy at READDIR/READDIRPLUS. Each demanded page borrows the existing
immutable view through a temporary stream, preserves its arithmetic cookies and
source identities, then releases that page's issued pins. Sync validates the
inode and retains ordinary backing-directory synchronization. Unadvertised
kernels and mutable policies retain their normal per-open streams.

Directory streams with ordinary handles store exact issued membership in sparse 64-bit masks,
using one hash record for up to 64 inode numbers per stream. Only returned
entries acquire lifetime references, including across partial pages and cookie
replay. Shared inode counts preserve metadata after FORGET until the last stream
that returned that entry closes. Source pages update masks by contiguous inode
range; merged views retain their version cursors and add only each returned
inode. Unread gaps allocate no masks. This bookkeeping loads no file payloads.
Local packed/delta decoding retains the documented libgit2 memory limitation.
Libgit2 can also retain decoded delta bases (up to 1 MiB each, 16 MiB per
pack); anonymous retention is not limited to active decoder buffers.
Source-pack mappings are a separate
cache: startup selects 8 MiB windows and a 16 MiB soft mapped-byte budget, as
well as the existing descriptor cap. This replaces libgit2's 64-bit defaults of
1 GiB windows / 8 GiB mapped bytes. Active windows and necessary delta decoding
can exceed those retained-cache budgets; they are not object-size or RSS limits.
The permanent 48 MiB packed regression verifies bounded idle
mappings and exact first/last bytes without decoding objects again.
Protocol packs and native decoder stdout can still need transient bounded-part
staging; that I/O and all native children remain inside total-cost measurements.
No global cache, recovery fallback or corrupt-spill retry is introduced.

## Asynchronous FUSE port (work in progress)

The Linux mount adapter now uses the pinned local async-fuser adaptation in
`crates/async-fuser`. One local event loop owns namespace state through RefCell,
up to 16 outstanding native futures, and reusable request buffers. The adapter
negotiates 128 KiB writes; the receive buffer also respects Linux's 8 KiB
minimum before INIT. Pending callbacks do not prevent independent requests from
running. Explicit unmount cleanup is awaited before runtime shutdown, and mount
identity prevents cleanup from targeting a later filesystem at the same path.
The original MIT licenses and separate diffs from both upstream sources are
included with the local crates.

Local and remote mounts under all four write policies connect lookup/header
demands, directory
preparation, read and readlink to `snapshot_async.rs`, `source_async.rs` and
`raw_blob_async.rs`. Trees, sizes, verified descriptors and the payload LRU belong
to the loop. No namespace borrow survives a source or disk await. Operations
needing an inode across a wait pin it against FORGET. Protected resident-source
reads keep their direct path without an inode gate or worker job.
A source boundary owns one decoder thread for libgit2's blocking API, with
bounded chunk transfer; it is not a worker pool used by every callback. The
local cache uses Rc-owned verified bytes. File operations use native io_uring
when available; otherwise one lazy bounded I/O worker handles them asynchronously
without moving the cache or namespace onto that worker.
A fitting working set performs no payload disk I/O, coalesces concurrent demand
for an OID, and serves RAM hits while the decoder waits. Shared local bytes stay
alive through FUSE reply completion without a payload copy. Session shutdown
drains requests, awaits the source thread and then publishes final metrics.
Objects up to 64 KiB cross the source boundary in one completely verified
response, using the verified ODB read directly instead of first probing an
unavailable packed-object stream. Larger objects retain bounded streaming.
Spill storage remains one file per OID.

Local sources also negotiate adaptive READDIRPLUS when the kernel supports it.
The initial page returns file attributes only when already available, without
cold leaf headers or private-file stats for names-only callers. Later adaptive
PLUS requests batch only the headers that fit their bounded reply page; they do
not load payloads or descendant trees. Headers and concurrent individual size
requests share the same per-OID gates/cache. Missing or unsupported metadata
keeps a listed name with nodeid zero and defers its error to ordinary access.
Old directory views never publish a replacement binding under an old inode.
Returned positive entries acquire lookup references, and namespace/cached inode
ownership survives reply completion. Mutable attributes retain zero TTL, while
all-policy name bindings can cache independently. Remote sources retain ordinary
READDIR because their protocol currently obtains sizes through blob demands.
Adaptive directory replies do not alter RAM-hit handling; small payloads remain
separate objects.

Repeated pages in mutable namespaces omit another positive reply for existing
metadata-only source leaves. Ordinary revalidation can still request their
attributes; this avoids duplicate lookup ownership and inode gates on repeated
stat-only scans. Content preparation retains the conservative positive-attribute
path because file reads may invalidate kernel atime, including cached reads.

Protected mutable policies also load pinned ignore rules through that same
payload cache. Compiled rules reuse the existing parser, matching semantics and
4 MiB/128-directory retention budget. Policy/source waits finish before a short
namespace commit, which receives explicit per-path decisions. An asynchronous
namespace gate orders these mutations without preventing warm reads/lookups
from progressing. Lookup reuses the parent's prepared name index for all three
policies; generated entries retain their normal writable namespace identity.
There is no second legacy payload cache on these native source paths.
Local request errors remain filesystem errors: a command may handle EIO for a
missing or unsupported object and continue using readable siblings. Such errors
do not become invocation-wide remote-transport failures.

Private regular-file read/write, open/create, attribute refresh and file/directory
sync in native policies now prepare operations under short state
borrows and await native I/O outside them. Active handles/operations retain one
shared descriptor without duplicating it for each request. Per-inode local gates
order writes, append, truncation and attribute updates; independent source reads
continue. Truncate, timestamp and filesystem-capacity operations use one lazily
created bounded I/O actor for syscalls unavailable through the supported native
ABI. A resident source hit never enters this queue. Shutdown closes the queue,
awaits its completion and joins the owned worker.

Unlink, link and rename validate prepared metadata before backing I/O and a
short namespace commit. Numeric backing opens/removal run asynchronously.
Under `all`, copy-up streams verified source ranges through the same cache to
the private file without a second payload cache. Shared per-inode read ownership
survives through the FUSE reply; copy-up/write waits for old replies to finish.
Directory renames preserve their source origins for later cold child access.
Closing an unchanged source description is a short loop-owned state change;
it does not wait for a different description's outstanding READ reply or start
file I/O. Copy-up still waits for source replies and enumerates the remaining
live descriptions when committing. Private descriptions retain their ordered
metadata refresh and close path.
Cold `all` lookup recomputes its visible path under the namespace gate after
waiting. Positive `all` name bindings cache independently of attributes.
Unchanged source attributes and directory attributes may also be cached: these
are owned by the loop and kernel namespace operations invalidate affected
attributes. LOOKUP and GETATTR retain namespace and inode read ownership through
send/drop, so namespace edits, copy-up and metadata changes wait for older cached
replies. Attributes are read again after acquiring ownership. Modified/generated
regular files keep zero attribute TTL for backing-file refresh, including
passthrough. Directory contents remain live. Protected source operations retain
their existing direct path. A readonly open of unchanged all-policy source adds
its handle without waiting for an exclusive mutation gate; a pending copy-up
includes that handle when it commits.
These readonly source descriptions also omit the no-op close FLUSH, including
when another description later copies up the inode. Explicit fsync/fdatasync
and writing descriptions retain their normal synchronization paths.

Newly created directories also enter the existing reverse inode/path index.
Resolving their children uses that index instead of scanning unrelated source
or generated paths. Rename updates and unlink removes the same binding.

The fallback shares the same file worker with truncate, timestamps and statvfs,
with a 16-job channel. It uses owned buffers and retains the original descriptor
through completion. Ordinary RAM hits neither start nor submit to this worker.
Shutdown closes raw readers and directory anchors before awaiting the shared
worker's completion. No per-request blocking pool or second payload cache is
introduced. Native file I/O retains its existing path.

Remote native Git/protocol work uses one owned source thread with a bounded
16-demand queue in `remote_async.rs`. Its verified small allocations and block
descriptors move into the same loop-owned cache, without copying or rehashing
resident bytes. Source-side headers prevent repeated fetches; parsed trees are
shared. Verified decoder output enters a handoff queue directly, including
already-required delta bases from a multi-object pack. It does not first enter
another payload LRU. The active output queue retains at most 16 MiB of charged
payloads; excess payloads spill individually. Its eviction cursor advances once
per queued object, and each handoff drains only newly prepared/returned objects
without scanning older descriptors. A multi-object delivery or one exceeding
the charged size of a 64 KiB object applies backpressure until the loop publishes
or drops it. Single small deliveries send a publication marker without waking
the worker for an acknowledgement round trip. The worker waits only for a
repeated demand for an unpublished object or the 16-delivery bound; unrelated
small cold demands can proceed. Active decoder allocations are separate from
the 16 MiB retained cache. Small files remain independent objects.

A cancelled handoff returns its verified ownership to the source through a
channel, without taking a source-cache lock or performing file I/O on the loop.
The exceptional cancellation path keeps unconsumed prepared bytes on disk for
reuse instead of fetching them again or retaining another RAM cache. New disk
reads authenticate their returned buffers. Admission/storage errors remain
observable after a remote command catches EIO. Shutdown drains requests, closes
the source intake, joins the native source and publishes its counters/failure
before destroying the filesystem. Native transport cleanup runs on its source
thread; the ordinary Snapshot retains final observer/recovery state.
Session startup errors also await source shutdown before dropping the filesystem,
including mount failure before INIT. Cleanup failures retain the startup error's
kind and are included in its diagnostic.

Complete cancellation/cleanup/resource verification and paired performance
acceptance remain required. Component tests and protocol coverage do not establish
issue #20 or merge readiness. No temporary profiler or benchmark harness is
included in the source tree.

## Integrity and source decoding

Every source object must match its canonical Git OID before publication.
Verified descriptors retain the original object identity, exact length and
ordered block hashes in trusted process memory. A disk reload checks complete
intersecting blocks and returns bytes from those same owned buffers. Reordering,
truncation, substitution and same-UID disk tampering therefore fail before the
changed bytes are served. Root and process-memory access are outside the
approved threat model. An exclusive file open alone is not an integrity proof.

The audited vendored git2 0.20.4 / libgit2 1.9.7 path already verifies a complete
owned ODB object with collision-detecting SHA1DC. The CLI explicitly enables
strict verification before Git work/threads. Only a typed owned blob with a
matching OID/type may reuse that proof; arbitrary streams, unknown dependency
builds and embeddings without that policy keep the independent Rust check.
Loose streams verify their complete payload. Copies remain cancellable in
64 KiB chunks, and trusted block hashes are formed from the same bytes before
publication. Forged pack-index mappings and raw-store corruption have separate
permanent negative regressions.

Remote imports retain verified raw bytes without unpacking/recompressing each
object into a loose Git object. An invocation owns and reuses its transport
session and prepared objects. Native fallback/import subprocesses, protocol
packs, staging, decoding and cleanup remain part of the full cost; no stage is
excluded from the interview's materialization-plus-command baseline.

## Permanent verification

- Rust raw-store tests cover block/length/identity corruption, failed publication,
  one retained cache entry per OID, active reader ownership, eviction, large
  streaming objects and closed rereads. Temporary files obey RLIMIT_FSIZE.
- `tests/remote.py`, `tests/ssh_regressions.py` and `tests/native_job_control.py`
  cover real transport, reduced descriptor limits, cancellation and same-run reuse.
- `tests/object_reuse.py` runs the production CLI over distinct objects. Both
  passes check exact data/stat results; actual Git argv and fetch/decode counters
  must agree, and no `unpack-objects` subprocess is permitted.
- `tests/metadata_reuse.py` checks wide directories with repeated stat/open/close
  demand. Each unique tree is prepared once without a new full traversal for
  every name; `tests/tree_metadata.rs` covers parser correctness.
- `tests/source_cache.py` compares real kernel callback counts with and without
  protected-source caching and native `all` name caching while checking identical
  namespace/content. Protocol tests separately keep mutable attribute TTLs zero.
- `tests/pack_scaling.py` and reduced-RLIMIT tests retain their specific resource
  and complexity assertions; they do not stand in for full invocation timing.
- `tests/real_repository.py` supplies the pinned WebKit full-namespace/two-pass
  workload and 32 real upstream tooling tests, complete native materialization
  baselines, direct-command timings, memory and transfer accounting.

Temporary profilers and their results are retained outside the mergeable source
tree. No profiling workflow, phase clock, inclusive span collector or profiler
CI step is part of this version. Test-only behavioral counters remain available
through `test-metrics`; those counters verify reuse, resource bounds and backend
execution rather than attributing latency to functions.

A green behavioral CI run does not establish the performance contract. Full
Git-on cost must still be compared with the same complete revision materialized
by Git plus the same command, under declared source/cache conditions. Large
constants and measured excess require investigation; no numeric allowance or
completed performance acceptance is inferred from asymptotic claims.
