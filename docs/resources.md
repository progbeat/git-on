# Resources and integrity

## Payload cache

Each invocation owns its prepared objects and temporary storage. The decoded
payload LRU has a 16 MiB budget, charged by allocation capacity plus 512 bytes
per retained entry. There is at most one retained entry per original Git OID
across all readers: a complete object up to 64 KiB or one block of a larger
object. Replacing that block does not invalidate an active read's owned buffer.
Active read buffers and necessary decoder allocations are accounted separately.

Small objects stay in RAM until eviction. Eviction saves their verified decoded
bytes in private raw storage before releasing them. Large objects stream to one
raw file per original OID and retain at most their most recently read 64 KiB
block. Prepared objects remain reusable after close/reopen and cache eviction
without another fetch or decode. Separate invocations have separate storage.
If caching an authenticated disk block would require a spill that fails with an
I/O error, the read returns its checked bytes without admitting that block.
The unbacked resident remains in RAM; the payload budget is not increased.

## Metadata and source decoding

Original OIDs, exact lengths and ordered SHA-256 block hashes remain in trusted
process memory. Touched unique directories are parsed once into shared immutable
structures; raw tree bytes are released. Parsed directories, object descriptors,
header sizes and filesystem namespace state grow with demand outside the
payload LRU. The 16 MiB payload budget is not a total-RSS limit.

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

Raw backing uses one file per prepared original object. Readers share a retained
raw descriptor rather than keeping an open file for each object. Disk usage
grows with prepared data, including evicted small objects and streamed large
objects. Protocol packs and native decoder output can also require transient
bounded-part staging. Temporary files obey inherited file-size and descriptor
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

In FUSE, metadata and namespace changes preserve the source OID without loading
or copying its payload. Read handles, hard links and unlinked inodes retain
access to that source. Writable opens, empty writes and unchanged-size truncation
leave source data intact. The first nonempty write or content-changing truncation
prepares private data; all handles then observe changes through the same inode.
The private file is sparse: it stores overwritten ranges while untouched bytes
come from the verified source. A read followed by a small write therefore does
not duplicate the complete raw object. Truncation permanently hides the removed
source tail; extending the file exposes zeros. Distinct inodes sharing a source
OID retain independent modifications.

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
