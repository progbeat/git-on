# FUSE callback acceptance matrix

This is the reviewed operation inventory for **registry fuser 0.18.0 and Linux ABI 7.40**.
It is a test contract, not an assertion that any particular CI run passed.
The dependency has 44 trait methods: 26 implemented by the Linux adapter and shared `Overlay`, 2 inherited
lifecycle methods, 13 inherited unsupported handlers, and 3 macOS-only methods.

## Evidence layers and limits

- `tests/fuse_protocol.rs` sends encoded FUSE requests through
  `fuser::Session::from_fd` over a private `SOCK_SEQPACKET` socket. The real fuser
  dispatcher invokes the real `Overlay`; reply bytes, errors and subsequent
  state are asserted. This is **protocol testing, not a kernel mount**. It makes
  stale handles, caller credentials, narrow reply buffers and callbacks normally
  bypassed by Linux reproducible. It does not emulate Linux's syscall semantics.
- `tests/integration.py --require-fuse --uv` exercises actual mounts and caller
  behavior. The existing file/directory/link matrices remain part of acceptance,
  alongside the targeted operation tests below. Missing FUSE must fail rather
  than turn a real-mount claim into a skipped test.
- Rust helper/unit tests additionally check policy, object loading, retention,
  inode reclamation, append/mmap behavior and disposable-layer cleanup. They do not
  replace either layer above.
- A test mapped to a callback is not a measured percentage of statements,
  branches or all possible inputs. Kernel caching, `DefaultPermissions`,
  capability negotiation and VFS prechecks may avoid a callback even when the
  corresponding syscall succeeds or fails. The inventory checker verifies
  completeness and references; it does not infer that a test invoked a callback
  or that its assertions are sufficient. Review and actual test execution are
  separate requirements.

## Reproduce the checks

After fetching/building the locked dependencies, run:

```console
python3 tests/check_fuse_coverage.py --self-test
python3 tests/check_fuse_coverage.py
cargo test --locked --test fuse_protocol
cargo test --locked --all-targets
python3 tests/integration.py --binary target/debug/git-on --require-fuse --uv
python3 tests/write_policy.py --binary target/debug/git-on --require-fuse
```

The checker resolves the dependency source with `cargo metadata --locked
--offline --filter-platform <rustc-host>`, deriving the host from `rustc -vV`.
Filtering prevents an already-built Linux checkout from needing unused wasm or
Windows dependency downloads. It verifies the locked version/checksum and reviewed trait-source
fingerprint, enumerates every method including platform exclusions, compares
the actual Linux adapter overrides and default handler classifications, and checks
that named tests still exist. A dependency or trait change requires an explicit
review and fingerprint update, not merely adding the new method's name here.

## Named tests

The short IDs below keep the matrix readable. References are machine-checked.

<!-- TEST IDS START -->
| ID | File | Test function |
| --- | --- | --- |
| P-life | tests/fuse_protocol.rs | protocol_lifecycle_lookup_forget_and_batch_forget |
| P-io | tests/fuse_protocol.rs | protocol_file_io_sync_and_stale_handles |
| P-no-open | tests/fuse_protocol.rs | protocol_readonly_no_open_negotiation_and_stateless_reads |
| P-cache-cow | tests/fuse_protocol.rs | protocol_all_source_data_cache_stops_after_copy_up_without_extending_ttls |
| P-dir-attrs | tests/fuse_protocol.rs | protocol_cached_source_directory_attrs_do_not_cache_generated_membership |
| P-create | tests/fuse_protocol.rs | protocol_creation_links_removal_and_rename_flags |
| P-dir | tests/fuse_protocol.rs | protocol_directory_pagination_sync_statfs_and_bad_handles |
| P-dir-partial | tests/fuse_protocol.rs | protocol_partial_directory_pages_reopen_cookies_and_source_inode_identity |
| P-dir-version | tests/fuse_protocol.rs | protocol_source_directory_snapshot_survives_rename_unlink_and_newer_handle_close |
| P-access | tests/fuse_protocol.rs | protocol_access_and_setattr_credentials_and_boundaries |
| P-optional | tests/fuse_protocol.rs | protocol_all_unsupported_callbacks_refuse_without_mutation |
| P-namespace | tests/fuse_protocol.rs | protocol_namespace_errors_and_atomic_rejections |
| P-bounds | tests/fuse_protocol.rs | protocol_zero_length_sync_writes_and_setattr_times |
| P-failures | tests/fuse_protocol.rs | protocol_backing_failures_propagate_without_namespace_insertion |
| P-metadata | tests/fuse_protocol.rs | protocol_create_metadata_stale_setattr_and_directory_stream_lifetime |
| P-partial | tests/fuse_protocol.rs | protocol_partial_backing_write_preserves_visible_progress |
| P-sparse | tests/fuse_protocol.rs | protocol_sparse_allocation_matches_backing_after_truncate_and_write |
| P-none | tests/fuse_protocol.rs | protocol_none_denies_supported_mutations_and_preserves_source |
| P-untracked | tests/fuse_protocol.rs | protocol_untracked_writes_unignored_paths_and_keeps_source_immutable |
| P-cow | tests/fuse_protocol.rs | protocol_all_copy_up_setattr_links_rename_and_whiteouts |
| P-unlinked-cow | tests/fuse_protocol.rs | protocol_all_setattr_on_unlinked_original_read_handle |
| I-snapshot | tests/integration.py | FuseTests.test_snapshot_ignores_dirty_index_worktree_and_untracked_files |
| I-file | tests/integration.py | FuseTests.test_file_operation_matrix_preserves_tracked_bytes |
| I-readonly | tests/integration.py | FuseTests.test_readonly_stateless_large_reads_dup_mmap_and_sync |
| I-cache-cow | tests/integration.py | FuseTests.test_all_source_cache_is_coherent_across_copy_up_mmap_and_namespace_changes |
| I-directory | tests/integration.py | FuseTests.test_directory_and_rename_operation_matrix_is_atomic |
| I-links | tests/integration.py | FuseTests.test_symlink_and_hardlink_lifecycle_matrix |
| I-lazy | tests/integration.py | FuseTests.test_lazy_metadata_does_not_load_unread_blobs_or_descendant_trees |
| I-large | tests/integration.py | FuseTests.test_large_snapshot_blob_random_and_sequential_reads_agree |
| I-exec | tests/integration.py | FuseTests.test_executable_mode_symlinks_and_repository_discovery |
| I-mmap | tests/integration.py | FuseTests.test_append_flags_and_shared_mmap_follow_current_open_description |
| I-mknod | tests/integration.py | FuseTests.test_mknod_regular_files_follow_policy_and_special_nodes_are_refused |
| I-sync | tests/integration.py | FuseTests.test_fsync_fdatasync_and_directory_sync_preserve_live_handles |
| I-statfs | tests/integration.py | FuseTests.test_statvfs_reports_coherent_backing_filesystem_capacity |
| I-rename | tests/integration.py | FuseTests.test_renameat2_noreplace_and_rejected_flags_are_atomic |
| I-readdir | tests/integration.py | FuseTests.test_readdir_pagination_and_seekdir_preserve_merged_entries |
| I-xattr | tests/integration.py | FuseTests.test_xattrs_are_unsupported_without_changing_files_or_directories |
| I-allocate | tests/integration.py | FuseTests.test_fallocate_and_unknown_ioctl_fail_without_changing_bytes |
| I-lock | tests/integration.py | FuseTests.test_posix_and_flock_locks_use_local_kernel_fallback |
| I-pollseek | tests/integration.py | FuseTests.test_poll_and_lseek_use_regular_file_kernel_fallbacks |
| I-copy | tests/integration.py | FuseTests.test_copy_file_range_kernel_fallback_preserves_offsets_and_snapshot |
| I-partial | tests/integration.py | FuseTests.test_partial_backing_write_and_sparse_blocks_remain_visible |
<!-- TEST IDS END -->

## Callback matrix

The original entries below preserve default `ignored` acceptance. Supplemental
`P-none`, `P-untracked`, `P-cow` and `P-unlinked-cow` exercise actual protocol
dispatch for policy denials, unignored creation, tracked copy-up, namespace
whiteouts and open-unlinked metadata. The real-mount four-mode suite above
additionally checks Linux syscall behavior; these are separate evidence layers.

`implemented` means an `Overlay` override. `inherited` means a supported fuser
lifecycle default. `unsupported` means the inherited handler replies `ENOSYS`
if dispatched; it does **not** mean the corresponding Linux syscall must fail.
`-` means no separate evidence in that layer, with the reason stated below.

<!-- CALLBACK MATRIX START -->
| Callback | Classification | Protocol tests | Mounted behavior tests | Assertions and limitations |
| --- | --- | --- | --- | --- |
| init | implemented | P-life, P-no-open | I-snapshot | ABI negotiation succeeds; Linux kernel passthrough is negotiated only with CAP_SYS_ADMIN and kernel support; otherwise ordinary FUSE. Readonly `none` negotiates NO_OPEN_SUPPORT only when advertised by the peer; mutable policies and older peers retain ordinary handles. Optional remote-lock, readdirplus and writeback-cache capabilities stay disabled. Every protocol fixture initializes a real session. |
| destroy | inherited | P-life | I-snapshot | DESTROY is acknowledged and the session thread joins successfully; no requests follow termination. Default hook has no application-side work; backing registrations are owned by open handles; cleanup uses separate lifecycle tests. |
| lookup | implemented | P-life, P-namespace, P-dir-attrs | I-snapshot, I-lazy | Existing/missing paths, bad parent, invalid components and overlong names; cold/missing-object behavior is also checked on mounts. `protocol_kernel_cache_only_retains_protected_source` checks positive caching only for policy-protected source names and attributes, including directories; generated names and `all` stay uncached. Directory membership/streams remain uncached. |
| forget | implemented | P-life, P-namespace, P-metadata | - | Forgotten inode becomes unavailable; excessive/unknown forget counts do not corrupt root; removed directory is reclaimed after its handles close. Kernel timing is not deterministic; unit tests also cover open-handle retention. |
| batch_forget | inherited | P-life | - | Default dispatch delegates to forget for multiple inodes; subsequent getattr returns ENOENT for each. Kernel batching is not assumed by mounted tests. |
| getattr | implemented | P-life, P-io, P-namespace, P-metadata, P-partial, P-sparse, P-no-open | I-file, I-links, I-partial | Attribute replies and inode existence; initial mode/ownership, stale file handle rejected; partial-write size and sparse allocation blocks match actual backing metadata; mounted size, mode and link-count assertions. `passthrough_unlinked_inode_keeps_backing_until_last_lookup_is_forgotten` and the real enabled/fallback suite cover metadata through O_PATH after unlink or replacement. |
| setattr | implemented | P-access, P-bounds, P-metadata, P-sparse | I-file, I-directory, I-partial | Owner/nonowner decisions, changed uid/gid, readonly/stale/mismatched handle and immutable rejection, size overflow, explicit/now times; sparse grow/shrink reports actual backing size/blocks; mounted chmod/chown-self/utime and path/fd truncate. Linux does not supply macOS creation/backup times or file flags. |
| readlink | implemented | P-io | I-exec, I-links | Exact tracked target and EINVAL for regular inode; mutable, renamed and dangling links and immutable write-through boundaries on mounts. |
| mknod | implemented | P-create, P-failures | I-mknod | Regular file, existing/disallowed name, special-node EPERM, missing backing and no namespace insertion; real umask/uid/gid and FIFO/socket refusal. Device nodes are not supported. |
| mkdir | implemented | P-create, P-namespace | I-directory | Directory creation, umask, duplicate and forbidden names; real nested creation, metadata and later removal. |
| unlink | implemented | P-create, P-namespace | I-file, I-links | Success, immutable/missing/directory denial; mounted shared inode and open-unlinked file survival. |
| rmdir | implemented | P-create, P-namespace | I-directory | Empty removal, nonempty/file/missing rejection; mounted immutable directory denial and recursive-move cleanup. |
| symlink | implemented | P-create | I-links | Real callback creates a symlink; mounted exact target, write-through, dangling link and namespace/policy denial assertions. Embedded NUL is not representable in a well-formed wire pathname. |
| rename | implemented | P-create, P-namespace | I-directory, I-rename | RENAME and RENAME2 dispatch; NOREPLACE success/collision, rejected exchange/whiteout/unknown flags, self/same-inode and type mismatch; unchanged source/destination on mounted rejection. |
| link | implemented | P-create, P-namespace | I-links | Regular hardlink, directory/zero-link rejection; shared inode, link counts, alias writes and protected source/destination behavior. `passthrough_link_reply_refreshes_backing_metadata_without_stat` checks size/blocks/mtime in the LINK reply after bypass writes, before GETATTR or close. |
| open | implemented | P-io, P-cache-cow, P-no-open | I-file, I-exec, I-cache-cow, I-readonly | Readonly tracked success and write rejection; directory/symlink type errors; mounted flags, append, truncation and execution. KEEP_CACHE applies to unchanged regular source opened readonly, including all-policy source until copy-up; NOFLUSH remains restricted to policy-protected source. Mutable handles use ordinary behavior. Negotiated readonly no-open replies ENOSYS; Linux then skips per-file open/release RPCs and retains immutable cached pages. All-policy entry/attribute TTLs stay zero. Protocol copy-up flags are checked locally; I-cache-cow requires a real mount for mmap and namespace coherence. |
| read | implemented | P-io, P-bounds, P-partial, P-sparse, P-no-open | I-file, I-large, I-partial, I-readonly | Exact bytes, holes, EOF, negative offset, stale/mismatched/write-only handle; written prefix remains readable after a real short backing write; sparse hole boundary, sequential/range reads and large-object behavior on mounts. |
| write | implemented | P-io, P-bounds, P-partial, P-sparse | I-file, I-mmap, I-partial | Offset/hole writes, stale/readonly handle, negative/overflow offset, zero length and sync flags; RLIMIT_FSIZE forces partial progress followed by EFBIG, with prefix/size/block visibility verified; append and mmap/current-description behavior on mounts. |
| flush | implemented | P-io, P-bounds, P-no-open | I-sync | Repeated flush keeps handle usable; stale handle rejected. Mounted dup-close preserves remaining open description. The kernel may skip close-flush for readonly protected source with NOFLUSH; mutable handles and explicit fsync retain their callbacks. Negotiated readonly no-open uses fh=0 and returns ENOSYS once, allowing the kernel to omit further flush requests. Flush is not a persistence/fsync guarantee. |
| release | implemented | P-io | I-sync, I-links | Subsequent use and repeat release of closed handle fail; mounted open-unlinked lifetime. Linux does not report RELEASE errors through close; negotiated readonly no-open omits RELEASE entirely. |
| fsync | implemented | P-io, P-no-open | I-sync, I-readonly | Data/full sync of writable and immutable handles; stale handle rejection; mounted fdatasync/fsync before and after unlink. |
| opendir | implemented | P-dir, P-metadata, P-dir-partial, P-dir-version | I-lazy, I-readdir | Directory handle succeeds and regular inode returns ENOTDIR; separate open handles share prepared views until namespace changes; older views keep names/cookies. Untouched source views borrow verified tree entries and reserve one inode range without per-child path/node allocations. Real merged lower/upper listing. |
| readdir | implemented | P-dir, P-metadata, P-dir-partial, P-dir-version | I-readdir | Tiny replies force pagination, exact names and EOF, negative offset and stale handle; mutation/rewind on an open enumeration snapshot and reads after rmdir; large flat mount listing, telldir/seekdir/rewind, repeated partial independent streams and d_ino/stat identity. Source identities remain stable across metadata eviction; they are never reassigned to another source path or generated inode. |
| readdirplus | unsupported | P-optional | - | ENOSYS using a directory handle; capability is not negotiated by normal init, so the ordinary mounted stream uses readdir. |
| releasedir | implemented | P-dir, P-metadata, P-dir-partial, P-dir-version | I-readdir | Release succeeds and subsequent read/release fail; open-removed directory finally reclaimed after releases and forget; mounted closedir and independent streams. Reclamation accounts only actual forgotten/removed nodes, plus fixed dot references; it does not scan unread children. Unit tests check metadata reclamation, late-name membership, shared-tree path identity and merged-view reuse. |
| fsyncdir | implemented | P-dir, P-failures | I-sync | Full/data sync, stale handle and missing backing error; root, tracked and writable directory descriptors on mounts. |
| statfs | implemented | P-dir, P-failures | I-statfs | Coherent statistics and missing backing error/recovery; mounted statvfs/fstatvfs capacity matches backing filesystem, with non-racy free-space inequalities. |
| setxattr | unsupported | P-optional | I-xattr | ENOSYS at protocol; Linux EOPNOTSUPP and cached repeat, unchanged file/directory contents and metadata. |
| getxattr | unsupported | P-optional | I-xattr | ENOSYS at protocol; Linux EOPNOTSUPP, including cached repeated requests. |
| listxattr | unsupported | P-optional | I-xattr | ENOSYS at protocol; Linux EOPNOTSUPP, including files and directories. |
| removexattr | unsupported | P-optional | I-xattr | ENOSYS at protocol; Linux EOPNOTSUPP and unchanged content/metadata. |
| access | implemented | P-access | I-exec | Owner/group/other/root and execute/write denials with synthetic credentials. DefaultPermissions bypasses this callback on real mounts; os.access verifies kernel-visible mode only. |
| create | implemented | P-io, P-failures, P-metadata | I-file | Create/open dispatch, initial umask/uid/gid, byte I/O and missing backing/no insertion; mounted exclusive collision, disallowed paths and open flags. |
| getlk | unsupported | P-optional | I-lock | ENOSYS if forced; POSIX lock capability is disabled, so mounted conflict/query/release behavior is kernel-local. |
| setlk | unsupported | P-optional | I-lock | Both SETLK and SETLKW dispatch return ENOSYS if forced; kernel-local POSIX/flock conflict and release behavior is tested separately. |
| bmap | unsupported | P-optional | - | ENOSYS if forced. This is a regular filesystem, not a block-device mount; no corresponding supported mounted bmap contract. |
| ioctl | unsupported | P-optional | I-allocate | ENOSYS for an unknown argument-free request; Linux ENOTTY for that request, no data mutation. Generic VFS-handled ioctls are outside this default-handler claim. |
| poll | unsupported | P-optional | I-pollseek | ENOSYS at protocol; Linux falls back to regular-file readable/writable readiness, including repeated calls. |
| fallocate | unsupported | P-optional | I-allocate | ENOSYS; direct Linux fallocate returns EOPNOTSUPP for allocation/keep-size/hole-punch without changing bytes. glibc posix_fallocate emulation is not callback evidence. |
| lseek | unsupported | P-optional | I-pollseek | ENOSYS for SEEK_DATA request; mounted DATA/HOLE fallback and EOF ENXIO. SET/CUR/END are kernel-handled and do not establish callback coverage. |
| copy_file_range | unsupported | P-optional | I-copy | ENOSYS at protocol; mounted Linux read/write fallback preserves explicit/implicit offsets and immutable source bytes. |
| setvolname | macos-only | - | - | Excluded by fuser target_os=macos cfg; git-on's supported platform and CI contract are Linux. |
| exchange | macos-only | - | - | macOS-only trait method; distinct from Linux RENAME_EXCHANGE, which is explicitly rejected and tested above. |
| getxtimes | macos-only | - | - | macOS-only extended-time method, absent from the Linux trait. |
<!-- CALLBACK MATRIX END -->

## What this does not claim

All Linux callbacks have a named dispatch test and a reviewed supported,
unsupported, default or mount-bypassed contract. That finite requirement is
different from exhaustive coverage of all argument combinations, kernel
versions, races or host failures. In particular:

- Deterministic missing-backing tests establish error propagation and atomic
  namespace rejection. An isolated `RLIMIT_FSIZE` regression limits the server's
  backing file: a six-byte write makes three bytes of real progress, that prefix
  remains readable with the correct visible size, and a later no-progress write
  returns `EFBIG`. Separate sparse grow/shrink and offset-write assertions compare
  visible size and allocated blocks with actual backing metadata. These are not
  simulations of every `ENOSPC`, `EINTR`, short-I/O cause, fsync media error or
  machine/power failure.
- Inode/link-count integer exhaustion is not forced by constructing billions of
  paths; bounded handle/path rejection cases are tested directly instead.
- macOS-only fields and operations, malformed wire parsing inside the third-party
  fuser library, block-device operation, and remote/distributed locking are not
  supported platform contracts.
- A green inventory check does not prove runtime execution. Acceptance must cite
  the actual exact-revision protocol and non-skipped real-FUSE results. Counts of
  passing scenarios must never be presented as a branch-coverage percentage.

## Directory link-count regression

`protocol_mutable_directory_links_follow_namespace_changes` covers mutable
parent link accounting through mkdir, rmdir, cross-parent rename, replacement
and same-parent rename while preserving tracked-parent metadata. The randomized
real-mount oracle in `tests/stress.py` also checks this after every operation.

## 1.3 passthrough evidence limits

The published registry fuser 0.18.0 owns Linux protocol handling and backing
registration. Its locked checksum and unchanged reviewed Filesystem trait are
checked above; no local fork or FUSE protocol implementation is shipped.
`src/backend/linux.rs` converts typed callbacks into the shared Overlay engine.
macOS mounts are deferred to issue #12. Mac CLI/materialization tests do not
establish FUSE mount behavior.

Linux passthrough uses FUSE_PASSTHROUGH, FUSE_DEV_IOC_BACKING_OPEN/CLOSE and
FOPEN_PASSTHROUGH for command-created regular files only, after policy and access
checks. Create/open handles share one BackingId per inode, retained through
release. Git-backed and copied-up tracked inodes never qualify. getattr refreshes
size, blocks and data timestamps from the backing descriptor without replacing
logical overlay mode, uid, gid or link count. Lack of the kernel capability,
CAP_SYS_ADMIN or a successful registration retains userspace I/O on this same
FUSE mount. No new privileged child, detached process or early teardown exists.

`tests/passthrough.py` must establish real negotiation and bypass, metadata,
reopen/truncate, executable and mmap behavior on a capable kernel. Protocol
responses and ordinary mount tests alone do not demonstrate kernel bypass.
