# Review regression evidence

Behavior fixes must retain a test of the original failure, not only a new happy
path. This ledger separates observed red-before/green-after evidence from
review hardening and tests whose real-mount execution awaits exact-head CI.

## Reproduced runtime defects

The independent review used `03f18c36fcb01f8d49cb196e250cee2bffd0f86a` as the
original revision. Its same three standalone counterexamples failed there and
passed with the corrected runtime. Permanent regression names below are part of
the repository test suite.

| Finding and original failure | Retained regression | Corrected assertion |
| --- | --- | --- |
| Backing write wrote `abc` then failed under `RLIMIT_FSIZE=3`, but visible size stayed zero and reads hid the prefix | `protocol_partial_backing_write_preserves_visible_progress`; `FuseTests.test_partial_backing_write_and_sparse_blocks_remain_visible` | Six-byte request reports three bytes; visible size/read/backing all agree; later no-progress write reports `EFBIG` |
| Sparse 1 GiB truncate reported 2,097,152 allocated blocks while backing reported zero | `protocol_sparse_allocation_matches_backing_after_truncate_and_write`; the same mounted regression | Mutable `st_blocks` equals actual backing allocation after grow, shrink and offset write |
| Ordinary text beginning with the LFS version line was rejected as an LFS pointer | `snapshot::tests::lfs_detection_requires_complete_valid_pointer_structure`; `snapshot::tests::lfs_prefix_in_ordinary_blobs_is_readable_through_both_read_paths` | Only complete supported LFS pointer structure is rejected; ordinary prefix-bearing bytes remain readable through both content APIs |
| Malformed `--cleanup` invocation returned runner failure 125 although help promised usage status 2 | `CliTests.test_invalid_arguments_never_execute_or_create_runtime_files` | Every listed malformed invocation returns exactly 2, without runtime artifacts |
| README claimed 256 compiled ignore-rule cache entries while runtime limit was 128 | `policy::tests::documented_ignore_cache_budget_matches_runtime` | Documentation contains the budget derived from the actual constants |

The partial-write regression changes process-wide limits only in an isolated
subprocess. Its mounted counterpart limits the server's backing writes and
restores limits in `finally`; it does not impose a product quota. The write and
truncate paths share backing-derived metadata refresh to prevent future drift.

## Verification infrastructure defects

- The first inventory check failed on clean CI because unfiltered offline Cargo
  metadata attempted to fetch an unused wasm dependency (`bumpalo`). The old
  command was reproduced in an isolated Cargo home lacking wasm archives/source;
  the host-filtered command passed in that same home. The checker self-tests
  require host selection, `--locked`, `--offline`, and `--filter-platform`, and
  reject inability to determine the host instead of silently retrying unfiltered.
- A runner unit test intermittently observed a lock still held after local close
  because another test fork briefly inherited the open description before exec.
  The actual failure was observed; no deterministic red schedule is claimed.
  Lock-lifetime and process-spawning tests now share a test-only serialization
  mutex; the existing runner suite passed 20 consecutive parallel repetitions.
- Independent coverage review strengthened INIT negotiation (offer capabilities
  before asserting they are declined), bounded directory pagination and required
  advancing cookies, parsed exact dirent names, and prevented commented-out Rust
  tests from satisfying the inventory guard. These are test-quality corrections,
  not claims that corresponding product behavior was previously defective.

## Release regressions

- Adding MIT `LICENSE` to the release archive exposed the original installer's
  exact three-member assumption. On master `a98e53f`, the new
  `InstallerTests.test_license_member_and_original_release_layout` passed its
  legacy case but rejected the licensed case with "unsafe paths". After allowing
  that one optional regular file, both cases pass. The companion
  `test_license_member_cannot_bypass_archive_checks` rejects duplicate, linked,
  and oversized license members. The package acceptance check compares the
  archive's license bytes with the source `LICENSE`.
- The README cleanup moves detailed cache budgets to `docs/usage.md`.
  `policy::tests::documented_ignore_cache_budget_matches_runtime` now checks that
  document against the same runtime constants, preserving its original guard.
- A failed GitHub upload can leave a `starter` asset. The original publisher
  treated it as complete and repeatedly failed its hash check. The reproducer
  failed before the fix and passes afterward in
  `PublicationTests.test_resume_failed_upload_starter_asset`; the companion
  `test_published_starter_is_never_deleted` ensures repair never changes a
  published release. Only an identity-checked draft's incomplete asset is retried.
- Comparing a master push to `HEAD^1` rejected a rebased multi-commit PR when its
  version bump preceded the last commit. The old comparison was reproduced as
  rejecting the fixture. `PushBaselineTests.test_rebased_multicommit_push_uses_before_not_last_parent`
  now validates the push event's actual before/after range; the workflow test
  checks both validation stages use that event. Version-tuple increment checks remain covered by the policy suite.

These release tests use isolated Git fixtures and a fake API state machine; they
never publish or delete a live release. See [release documentation](releases.md).
The final verification report must identify which of these tests ran on the exact
published revision; a prior green workflow is not evidence for later changes.

## Ongoing acceptance

The [FUSE matrix](fuse-coverage.md) lists protocol and mounted evidence separately.
Seeded bounded random stress supplements these focused cases. Preserve the seed
and operation journal for any new failure, minimize it to a focused reproducer,
then retain that reproducer with the fix. A finite suite cannot prove all possible
inputs, hardware failures or thread interleavings.

## CLI retention flags and command boundary

The 1.0 syntax reserves an initial post-revision `--` boundary and adds `-k`.
Ordinary separator-free calls remain literal; dash-named executables in the
newly reserved forms require the explicit escape documented in [usage](usage.md#command-syntax).
This is a documented compatibility change, not a claim that every old literal
invocation was a defect.

Red-before was run against the published v0.1.1 Linux x86_64 archive after
checking both SHA256SUMS entries and `release.json`'s source commit
`257d50f10aa461bc7c9e57ae97aaaf8604e9f6da`. In an isolated Linux container,
`CliTests.test_explicit_option_errors_never_resolve_tree_or_execute` produced
13 failing subcases: unknown explicit options, empty revision/command names,
and missing commands after the new separator returned 125 rather than 2.
The old binary tried revision resolution or mounting; this container had no
FUSE device, so this red result makes no real-mount claim.

Six parser unit tests pass locally, including both keep spellings/positions,
repetition, first-boundary handling, legacy literal argv, revision escapes,
and empty/non-UTF-8 arguments. The retained black-box regression checks usage 2
before Git resolution, child execution, or runtime-state creation. Real-FUSE
acceptance in `FuseTests.test_keep_spellings_and_positions_retain_real_subtree_writes`
checks generated bytes under `backing/`, unmount and pin removal, and refused
cleanup of retained state for prefix/suffix long/short forms. The existing stream
and non-UTF-8 tests cover both command syntaxes; the locked uv/Numba examples
exercise both suffix spellings. Green-after is confirmed in
[CI run 38123391582](https://github.com/progbeat/git-on/actions/runs/38123391582)
at `dad2ef2606709365026818538b9df70c4cabfbd6`: the negative CLI regression passes
in both the debug and installed distribution, and all 64 real-FUSE/uv integration
tests pass without skips. The five required suites passed, including the
callback/protocol guard, stress and million-file workload. The packaged-binary
job repeats real-FUSE checks; its optional uv test is skipped because uv runs
in the separate real-FUSE job. Later commits still require fresh exact-head CI;
this recorded run is evidence for the named revision alone.

## Mutable directory link counts

The randomized differential oracle identified that mutable directories retained
`nlink=2` after child directories were created. The focused wire test
`protocol_mutable_directory_links_follow_namespace_changes` failed before the
fix (`2` versus `4`) and passes afterwards. It checks creation, removal,
cross-parent moves, directory replacement, same-parent rename, the removed
inode's zero links, and unchanged tracked-parent metadata. Parent accounting is
incremental and does not traverse the snapshot. The real-mount randomized suite
also compares mutable directory link counts after each operation.

## Stress harness symlink containment

Independent review demonstrated that relocating a relative symlink to a shallower
path could make the harness follow a target outside its fixture. This was a test
harness issue, not a product sandbox guarantee. The retained self-test
`test_relocated_symlink_cannot_read_external_sentinel` verifies that relocated
links cannot expose the controlled external sentinel. The operation validation
now accounts for resolved link targets before following them.

## Installer archive metadata

Independent installer review reproduced acceptance of a 16,567-byte compressed
archive containing a hidden 16 MiB PAX extension record before the three expected
members. The original tarfile iteration consumed the extension before member
validation, bypassing the intended metadata bound. The fixed reader first bounds
total gzip expansion, then checks raw tar headers and only copies the expected
regular executable bytes. It rejects PAX/GNU extensions, sparse/contiguous-file
types, unsafe paths, duplicate names, links and oversized members.

`InstallerTests.test_hidden_extended_headers_and_unexpected_types_are_rejected`
retains the focused rejection cases;
`test_expanded_archive_and_readme_limits` checks expansion and member limits.
Both passed after the fix. These offline fixtures do not make network requests
or install outside their private temporary test directories.

Independent review also found that a truncated curl-to-shell stream could end
at a syntactically valid Python exception-handler boundary and still replace the
old executable. `test_truncated_pipe_payload_never_starts_installation` reproduced
six failing suffix boundaries before the fix, including actual replacement, and
passes afterward. The complete heredoc now belongs to a shell function that is
invoked only after its closing brace has been received. The ordinary complete
pipe, repeated-installation and update paths remain covered.

Malformed release asset names (a JSON list/object instead of a string) originally
raised an unhandled TypeError. The metadata-shape regression reproduced that
traceback before the type guard and now confirms a concise failure while the old
binary remains unchanged.

A follow-on truncation check showed that a bare final function name at EOF could
run without forwarding the requested version, destination or authenticated mode.
The final invocation is also enclosed in a complete shell group.
`test_final_invocation_truncation_cannot_drop_arguments` reproduced dropped
arguments before that guard and now rejects every incomplete byte-prefix of the
call while verifying argument preservation for the complete script.

Final package-log inspection confirmed dynamic libz.so.1 and libgcc_s.so.1
requirements in addition to glibc. The missing-runtime-library probe test failed
before the diagnostic improvement and passes afterward; the existing executable
remains unchanged. README and installation guidance now list those runtimes.
