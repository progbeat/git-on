# Resources and integrity

## Payload cache

Each invocation owns its prepared objects and temporary storage. Mounted runs
share a 16 MiB LRU for complete blobs up to 64 KiB and authenticated blocks read
from spills. Each entry is charged by allocation capacity plus 512 bytes.
A separate LRU retains the eight most recently used complete large blobs.
Readers of one OID share a resident buffer. Active read buffers and decoder
allocations are accounted separately.

An additional 16 MiB LRU holds compressed copies of unrequested remote delta
bases that might be needed later. Entries are charged by allocation capacity
plus 512 bytes. The decoder keeps any base needed by its active pack until
the pack completes. One decoded base of up to 16 MiB can be reused within
each active pack. A later demand uses the compressed copy when retained.
On eviction the unused compressed copy is released. A later first demand
can fetch it directly.

Evicted payloads can spill to private storage. Prepared objects remain reusable
after close/reopen and eviction
without further fetch or decode. If admitting a checked disk block would
require an unrelated spill that fails, the read returns the checked bytes and
keeps the previous resident.
If storage capacity prevents an eviction write, its resident bytes remain
readable and the failed remote read can be retried once space is available.

Reads from a whole resident blob hold the cached buffer until the reply has
consumed the requested range. The FUSE reply sends that range directly.
Local packed objects use libgit2 when available. The native batch reader
streams objects from local stores that libgit2 cannot read.

Mounted HTTPS reads decode responses outside the network dispatcher. A large
victim stays readable in memory while its spill is written. Independent
requests can complete during that write. Each response has a bounded input
queue and pauses its own HTTP/2 stream when the queue fills. Foreground
reads and read-ahead have separate limits on concurrent decoders.

## Metadata and source decoding

Original OIDs, exact lengths and ordered BLAKE3 block hashes remain in trusted
process memory. Touched unique directories are parsed once into shared immutable
structures; raw tree bytes are released. Parsed directories, object descriptors,
header sizes and filesystem namespace state grow with demand outside the
payload LRU.

Libgit2 uses 8 MiB source-pack mapping windows and a 16 MiB soft mapped-byte
budget. Retained pack descriptors are limited to
`clamp(floor(RLIMIT_NOFILE / 4), 1, 32)`. Git-on does not raise OS limits.
Active windows and necessary packed-object/delta decoding can exceed retained
cache budgets. Libgit2 can retain decoded delta bases up to 1 MiB each and
16 MiB per pack. Packed/delta decoding can require a complete decoded object;
these budgets are not object-size or total-memory ceilings.

The wholly readonly `none` policy permits kernel directory-page caching on FUSE
ABI 7.28 and later. Mutable policies keep directory contents live. Cached pages
cover enumerated directories and do not preload leaf contents or subtrees.

## Files and temporary disk space

Raw backing uses one file per spilled original object. The store shares one
recent read descriptor. Git-on decodes incoming Git packs from HTTPS and OpenSSH
streams, keeping active decoder state in memory. Temporary disk usage is
bounded by the selected objects that spill and the work the command requires.
Private Git scratch may be used for index revision resolution and native
signature verification.

Temporary files obey inherited file-size and descriptor limits. Git-on catches
`SIGXFSZ` from its own writes so `EFBIG` follows normal cleanup. Executed commands
retain the caller's signal disposition. Stale runs follow the documented
recovery contract.

The copy backend writes verified local data into the private execution tree.
For a network source it requests a shallow pack for the selected revision.
A 128 MiB payload LRU retains verified blobs until materialization consumes them.
An evicted file's backing can be transferred into the visible tree within the
same private run. Git-on checks the installed file and its block digests before
launching the command.
Repeated OIDs can reuse a prepared file or symlink target across directories.
Copies remain independent. The command starts after the tree passes verification
and policy checks.

In FUSE, metadata and namespace changes preserve the source OID without loading
or copying its payload. Read handles, hard links and unlinked inodes retain
access to that source. Writable opens, empty writes and unchanged-size truncation
leave source data intact. The first successful write or content-changing
truncation commits private backing. A failed operation that changes no bytes
leaves the source backing available for verified reads and retry. All handles
observe committed changes through the same inode.
Before the first destructive source operation, an ownership index counts original
file and symlink inodes in the selected tree, including unopened paths. Shared
trees are counted by multiplicity. This reads tree metadata, never blob contents;
remote tree demands are batched by depth. It is not startup or readonly work.

If another original inode or an active decoder still needs the raw source,
copy-up gives the mutated inode its own backing. It uses a filesystem clone
where supported, or copies the bytes directly. For the last original inode,
Git-on can transfer the raw file into the writable layer with its existing
descriptor. Its raw name is retained until the first successful change.
Resident bytes are written once. Truncation to zero releases the
original reference without loading its bytes. Unlink releases data after the
last kernel lookup or open reference. Truncation followed by extension exposes
zeros.

For a spilled blob, copy-up verifies the original block digests before
committing the writable backing. A direct copy verifies the same bytes it
writes. A linked or cloned backing is verified in place. Writes, truncation
and reads on the committed writable backing use ordinary file I/O. Linux can
use kernel passthrough on later opens after cached handles are released and
when it can preserve the file's attributes.

There is at most one decoded backing per live source inode, with unchanged
same-OID inodes sharing a single backing. No extra immutable full copy is retained
for an inode that has become writable. Prepared bytes still needed by any
original inode survive eviction and close/reopen without refetch or repeat
decoding. Necessary delta bases that are not requested source files remain in
compressed Git storage rather than becoming additional decoded raw files.

`spool_writes` counts raw payload `File::write` calls, including failed attempts.
`spool_bytes` counts bytes accepted by those writes, including partial data that
is later discarded. Transport/metadata staging and durability work are excluded.
The separate retained-raw counters describe current storage, not cumulative I/O.

## Integrity

Every source object must match its canonical Git OID before publication.
The pinned git2/libgit2 build uses collision-detecting SHA1DC; the CLI enables
strict native verification before Git work or threads. Only a typed owned ODB
blob with matching identity/type can reuse that proof. Streams and builds
without that policy use independent verification.

Trusted block hashes are formed from the same verified bytes. A reload of an
immutable object from its disk cache checks complete intersecting 64 KiB blocks and returns
bytes from those checked buffers. Length changes, reordering, substitution and
same-UID disk tampering fail before changed bytes are served. Resident verified
bytes are not hashed again on every read. Root access and interference with
process memory are outside the threat model.

## Verification requirements

- Raw-store tests cover identity, length and block corruption, failed publication,
  eviction, active readers, large-object streaming and inherited file-size limits.
- Remote, SSH and lifecycle tests check transport reuse, cancellation, cleanup
  and reduced descriptor limits.
- Object and metadata reuse tests check exact results, source demands, bounded
  cache accounting and preparation of touched directories without repeated scans.
- Real FUSE tests check kernel cache behavior and identical namespace/content
  results. The WebKit workload runs a small upstream command on the large public
  source and bounds its actual metadata, decoded-payload and pack demand.

Performance acceptance includes startup, native children, transfers, decoding,
verification, metadata work, temporary I/O and cleanup. Compare the same command
and complete revision under declared source and OS-cache conditions. Report
payload-cache use, metadata, active allocations, process memory, descriptors and
temporary disk use separately. Functional test success alone does not establish
performance acceptance.
