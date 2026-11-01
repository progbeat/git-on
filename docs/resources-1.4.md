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
The permanent 48 MiB packed regression verifies bounded idle
mappings and exact first/last bytes without decoding objects again.
Protocol packs and native decoder stdout can still need transient bounded-part
staging; that I/O and all native children remain inside total-cost measurements.
No global cache, recovery fallback or corrupt-spill retry is introduced.

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
  protected-source caching while checking identical namespace/content.
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
