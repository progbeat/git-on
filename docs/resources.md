# Resources and integrity

## Payload cache

Each invocation owns its prepared objects and temporary storage. A 16 MiB
block LRU charges allocation capacity plus 512 bytes per retained entry. It
holds whole blobs up to 64 KiB and at most one recently read block per spilled
blob. Newly verified larger blobs enter a separate whole-blob LRU with room
for eight entries. Reads update recency while an entry stays in that cache.
The first read can use the decoded buffer without writing and rereading a
raw file. After a spill, reads use the backing file and the block LRU.
Even a partial read can require a whole blob in RAM.

The count limit targets source repositories where a few whole blobs fit
comfortably in RAM. The payload bytes in the hot LRU are bounded by the
combined uncompressed size of the eight largest distinct blobs in the tree.
At the revisions below, this is about 14.35 MiB for CPython, 16.91 MiB for Go,
and 120.97 MiB for Linux. [CPython `de54cf5`](https://github.com/python/cpython/tree/de54cf5be371a6f5e2e9f208c38def5f81d3ef02) ·
[Go 1.24.0](https://github.com/golang/go/tree/3901409b5d0fb7c85a3e6730a59943cc93b2835c) ·
[Linux v6.12](https://github.com/torvalds/linux/tree/adc218676eef25575469234709c2d87185ca223a)

On trees with blob sizes like these, keeping a few verified buffers whole
avoids temporary writes and disk reads when a command revisits them. A
pending spill and active decoding buffers are additional memory costs.

Eviction spills verified data to private backing when needed. One large spill
can run in the background while readers use verified buffers. If that write
fails, the blob stays readable in RAM and the next large-blob admission
retries it before starting another eviction. Incremental HTTPS and SSH pack
decoders reserve a decode slot before inflating a large blob. One of the
eight slots stays available for required reads when prefetches occupy the
rest. Native Git decoding can allocate whole-blob buffers outside those slots.
A read can use its verified buffer while placement waits, including after
joining an in-progress prefetch. Network requests can run concurrently.
Prepared objects remain reusable across close/reopen and RAM eviction without
another fetch or decode. If an optional disk-block admission needs a spill that
fails, return the checked bytes and keep the unbacked resident.

## Metadata and source decoding

Trusted RAM retains object IDs, exact lengths and a 32-byte BLAKE3 digest per
64 KiB block of each spilled object. Full-block digests cost 1/2048 of the
payload size. Touched trees are parsed once into shared immutable structures.
The whole-blob cache, active reads, full-object decoding buffers, verification
metadata and namespace state are accounted for outside the 16 MiB block budget.

Libgit2 uses 8 MiB source-pack mapping windows and a 16 MiB soft mapped-byte
budget. The pack descriptor cache retains up to 32 descriptors.
Active windows and necessary packed-object/delta decoding can exceed retained
cache budgets. Libgit2 can retain decoded delta bases up to 1 MiB each and
16 MiB per pack. Packed/delta decoding can require a complete decoded object;
these budgets are not object-size or total-memory ceilings.

The wholly readonly `none` policy permits kernel directory-page caching on FUSE
ABI 7.28 and later. Mutable policies keep directory contents live. Cached pages
cover enumerated directories and do not preload leaf contents or subtrees.

## Files and temporary disk space

Each spilled object has one raw file. The store retains one last-read file
descriptor shared by its readers. Reusable HTTPS and SSH sessions decode full
objects and deltas in RAM from the first pack.
Readers can receive a verified object while the rest of the pack is still in
flight. Native Git fetches can use temporary pack files. Native decoder
requests, Git's configuration and discovery output,
and `mount_nfs` diagnostics pass through streams. Capture for
operations that can invoke detached hooks or SSH proxies uses a seekable handle
so inherited output remains writable after the native Git process exits.
On Linux the handle uses an anonymous memory file when available. Lazy
decoding fills the object buffer in RAM and verifies the full Git OID before
serving or spilling it. Reads contained in a resident blob or one authenticated
block share its buffer with the filesystem backend. Reusable pack decoders
complete split headers before passing the compressed body to zlib by reference.
The native ref capability check discards the ref listing and retains the
separate packet trace for compatibility with detached helpers.
Flat UTF-8 object database overrides configure libgit2 directly. Nested
alternates and non-UTF-8 paths use temporary aliases for the git2 binding.
The direct reusable HTTPS and SSH startup paths borrow verified commit or tag
headers in memory. Native Git fetches, native preparation and signature
verification can require a private temporary repository. Once the root is
prepared, remote subtrees are kept as parsed directory metadata without
writing loose tree objects.
The host enforces inherited process resource limits. See the
[resource assumptions](specification.md#resource-assumptions) for the
supported workload.
Successful cleanup releases all invocation-owned storage. Failed cleanup
follows the documented recovery contract.

Explicit-all materialization verifies object bytes directly into the
private execution tree, without a second complete raw copy. Loose objects
stream; packed/delta objects retain libgit2's required decoding. A repeated OID
reuses a prepared file or symlink target across directories, authenticating the
copied bytes against that original OID. File copies remain independent. The
command starts only after every destination has passed verification and policy
checks. Remote materialization isolates each received compressed pack. Only the
pack currently being decoded has an open ODB; it is released with its indexes
after its last needed blob reaches the execution tree. Blobs up to 64 KiB use
the ODB transfer; larger blobs stream from
native Git through the same OID verifier into their destination, without a
decoded raw spool. Repeated OIDs reuse verified files or symlink targets in the
execution tree. Lazy FUSE reads retain the invocation-owned verified raw store.

On native mounts, metadata and namespace changes preserve the source OID.
Writable opens, empty writes and unchanged-size truncation leave the source
payload untouched. The first content change installs a private file after
the change succeeds. Copy-up uses a filesystem clone when available and copies
the retained bytes directly otherwise. Truncating to zero starts empty. An
allowed writer can change any part of the private file, so all its bytes use
ordinary I/O, including bytes retained from the source. All handles of the
inode then see the private data. Eligible Linux FUSE opens can use passthrough.

Unlink preserves open or looked-up inodes until their last reference is
released. The immutable source remains reusable from the cache for the
invocation. The disk-space acceptance case allows two checked-out copies of the
selected revision and enough space for the same command to finish.

`spool_writes` counts raw payload `File::write` calls, including failed attempts.
`spool_bytes` counts bytes accepted by those writes, including partial data that
is later discarded. Transport/metadata staging and durability work are excluded.
The separate retained-raw counters describe current storage, not cumulative I/O.
`hot_blob_spill_bytes` measures the whole buffer held by an unfinished eviction.
`verified_decode_buffer_bytes` measures published pack buffers awaiting cache placement.

## Integrity

Every source object must match its canonical Git OID before publication.
The pinned git2/libgit2 build uses collision-detecting SHA1DC; the CLI enables
strict native verification before Git work or threads. Only a typed owned ODB
blob with matching identity/type can reuse that proof. Streams and builds
without that policy use independent verification.

Trusted block hashes are formed from the same verified bytes. When reading a
spilled immutable Git object from disk, Git-on checks its length and the
complete intersecting 64 KiB blocks and returns bytes from those checked
buffers. Length changes, reordering, substitution and same-UID disk tampering
fail before changed bytes are served. Resident verified bytes are reused
directly. Root access and interference with process memory are outside the
threat model.

## Verification requirements

- Raw-store tests cover identity, length and block corruption, failed publication,
  eviction, active readers and retry after a partial background write.
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
