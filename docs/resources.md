# Resources and integrity

## Payload cache

Each invocation owns its prepared objects and temporary storage. A 16 MiB
block LRU charges allocation capacity plus 512 bytes per retained entry. It
holds whole blobs up to 64 KiB and at most one recently read block per spilled
blob. A separate LRU retains the eight most recently used larger blobs whole.
Newly verified larger blobs enter this cache. A hot read uses the verified
buffer without writing a raw file.

Eviction spills verified data to private backing when needed. One large spill
can run in the background while readers use verified buffers. Another large
admission can wait for that spill once the hot cache is full. HTTPS and SSH
decoders reserve one of eight active buffers before inflating a large blob.
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

Each spilled object has one raw file. The store retains one last-read file
descriptor shared by its readers. Compressed protocol packs and decoder requests
can need transient bounded staging. Lazy decoding collects bytes in RAM and
verifies the full Git OID before serving or spilling them.
Temporary files obey inherited file-size and descriptor
limits. All invocation-owned storage is released by successful cleanup; failed
cleanup follows the documented recovery contract.
Git-on catches `SIGXFSZ` from its own writes so `EFBIG` follows normal error
handling and cleanup. Executed commands retain the caller's default or ignored
signal disposition.
Eager remote demands use individual exact-object requests under a finite
inherited file-size limit: a want count cannot bound native Git's resulting
pack size. Unrestricted runs retain batching. Request stdin is streamed through
a channel; decoder request staging uses bounded parts.

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
the retained bytes otherwise. Truncating to zero starts empty. All handles of
the inode then see its private data through ordinary I/O. Eligible Linux FUSE
opens can use passthrough after copy-up. Block authentication applies when
reading the immutable cache entry.

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

Trusted block hashes are formed from the same verified bytes. A reload from
mutable temporary storage checks complete intersecting 64 KiB blocks and returns
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
