# Review regression evidence

## Follow-up review of the published 8d6c02e checkpoint

Focused tests reproduced three current-head defects before their fixes:

| Defect | Permanent regression | Observed before | Required after |
| --- | --- | --- | --- |
| Peeled tag helper selected as an exact ref | `RemoteTests.test_review_peeled_tag_advertisement_rows_are_not_selectable_refs` | Both verified and unverified selection succeeded; both native signatures were valid, so verification silently selected the peeled commit | Reject the helper before fetching objects; the real tag remains selectable |
| Signal consumed before cancellation registration | `tests::review_signal_before_first_native_git_is_not_lost` | Real SIGTERM was observed before the first native Git call, which still started and blocked on a config FIFO; bounded test failed | Cancel before spawning Git; concurrent first operations also cancel and are reaped |
| Shutdown ignores selected SSH executable outside PATH | `SshRegressionTests.test_selected_ssh_client_outside_path_closes_master_on_drop` and `test_selected_ssh_client_outside_path_closes_master_on_stale_recovery` | All six GIT_SSH_COMMAND, GIT_SSH and core.sshCommand combinations authenticated/read successfully but left the exact master alive after deleting the run directory | Close the owned master with the selected executable during normal and stale cleanup |

These are real native Git and disposable OpenSSH reproductions, not transport
mocks. A pidfd identifies the exact disposable master. Shutdown now records
only canonical executable provenance, never the SSH command/configuration or
credentials. Changed or malformed provenance retains the owned socket and pin
with a visible failure; restoring it allows recovery. A dead owned endpoint is
reconciled only after its socket identity is checked again. The older mandatory
Git dependency finding is stale: the permanent local unverified test passes
with an empty PATH, including ancestry, subtrees, replacements and 70 tags.

Actual-FUSE tests additionally replay `8d6c02e` for ignored SSH shutdown errors:
the old runner returns 0 and removes recovery while its master remains alive;
the fixed runner returns 125, preserves the exact pin/socket, and succeeds after
the client is restored. CI conditionally builds that authentic ancestor; after
a squash merge, permanent current-behavior tests still run without history.

SIGKILL lifecycle observations cover blocked native configuration before
mounting, native helper completion after mounting but before command spawn,
and mounted command runtime. They distinguish released kernel locks from
retained pins, private objects, raw writable bytes and live child/master
processes. No cleanup handler can run under SIGKILL. External fixture cleanup
stops captured processes before checked stale recovery. The mount cases require
actual FUSE in CI; local skips are not execution evidence. The native helper
gate uses CI's unprivileged fusermount route; a privileged direct mount bypasses
that gate. SIGINT/TERM/HUP/QUIT startup and mounted-runtime cases remain separate.

Binary-size experiments remain outside the PR: no profile or clap feature
policy change is included in this follow-up.

## Remote/verification checkpoint: unfinished and unpublished

The initial uncommitted implementation was independently reviewed in the saved
Linux runtime. This checkpoint is not ready and has no real-FUSE or CI evidence.

`RemoteTests.test_review_configuration_values_are_applied_once` initially failed
with two `once` HTTP headers for one configured value, and now passes.
`test_review_stalled_transport_cancels_and_cleans_descendants` initially timed out
after two seconds; it now exits by SIGTERM, stops the transport group and cleans
temporary state. A non-reaping container PID 1 may retain an adopted dead zombie;
the check distinguishes that state from a live descendant.

Independent review reproduced repeated insteadOf rewriting: native Git failed
at a nonexistent first rewritten path while the loader reached a second path.
`test_review_url_rewrites_are_applied_once_for_urls_and_remote_names` passes after
pinning the once-rewritten URL. Review also showed 300 MiB of abandoned staging
data survived a successful cache load. The focused staging-recovery and concurrent
cache-bound tests pass after reconciliation under the writer flock.

`test_review_named_remote_preserves_uploadpack_configuration` initially succeeded
despite a configured nonexistent upload-pack command, failing its assertion. It
now passes after preserving the remote name/options with a pinned private URL.

`test_review_connection_loss_cannot_reconnect_with_explicit_proxy_option`
reproduced a reconnect after owned-master loss before the fix and now passes
with owned control settings and existing-socket mux proxy mode. The appended SSH
section records additional short-option and signal regression evidence. Final
independent re-review and exact-head real-FUSE CI remain publication requirements;
the new mounted EIO and recovery cases have not run in the local runtime.

Behavior fixes must retain a test of the original failure, not only a new happy
path. This ledger separates observed red-before/green-after evidence from
review hardening and tests whose real-mount execution awaits exact-head CI.

## Remote reference ambiguity and literal subtree paths

Independent review of the restored remote feature reproduced two additional
loader defects. A branch and lightweight tag both named `main` at the same OID
were accepted as unambiguous because resolution deduplicated by OID. A selected
`main:deep/.` subtree was also accepted because `Path::components` normalized
the dot away. Native Git rejects `HEAD:deep/.` and `HEAD:deep//`, while accepting
`HEAD:`, `HEAD:deep` and `HEAD:deep/` for the corresponding root/directory.

The permanent tests `review_same_oid_branch_and_tag_are_still_ambiguous` and
`review_subtree_components_are_not_silently_normalized` in
`tests/remote_source.rs` both failed before their fixes: `unwrap_err()` received
an `Ok(Snapshot)` for the ambiguous short ref and interior-dot subtree. The
focused command `cargo test --locked --test remote_source review_` exited 101.
After retaining reference names during ambiguity checking and selecting literal
subtree components, `cargo test --locked --test remote_source` passed all five
tests. These checks use actual filtered upload-pack through disposable `file://`
repositories and establish loader behavior; they do not claim real FUSE evidence.
Explicit `refs/heads/main` and `refs/tags/main`, an empty subtree and a single
trailing directory slash remain accepted.

## Cancellation while a competing writer holds the remote cache

Independent review held the persistent cache's flock in another process, started
the actual runner against a disposable filtered `file://` server, then sent
SIGTERM before mounting. The runner stayed alive for more than two seconds and
only exited after the competing lock was released. The permanent test
`review_waiting_for_cache_lock_is_cancellable` failed before the fix with
"runner ignored SIGTERM while another writer held the cache lock" (exit 101).

Cache locking now retries nonblocking flock while checking the same cancellation
signal used by owned Git subprocesses. The regression retains the competing
lock throughout cancellation and asserts SIGTERM status and removal of temporary
run state. After the fix, all six `remote_source` tests passed, including this
actual-binary startup check. This is local process/cancellation evidence without
a FUSE mount; cancellation during mounted remote reads still requires CI.

## Native configuration cancellation and owned SSH recovery identity

Independent re-review reproduced a native Git startup stall at the authentic
feature checkpoint `92377e27e32ac424d688ae8bcf0fbe6be0081c0c`. With
`GIT_CONFIG_GLOBAL` naming a FIFO without a writer, the runner's direct
configuration capture ignored SIGTERM for more than two seconds. The permanent
`SshRegressionTests.test_native_config_fifo_cancellation_stops_git_and_removes_state`
failed before the fix. Configuration capture and original Git-directory lookup
now use the same owned, cancellable subprocess path as transport and verification.
The regression asserts a real Git config child starts, the runner exits by
SIGTERM, the child stops and temporary state disappears.

The same review demonstrated that stale cleanup followed `ssh-control` symlinks
and stopped an unrelated disposable master outside its run. A type-only socket
guard fixed that case but still accepted a hardlinked/replaced real socket:
the external master's native `ssh -O check` changed from 0 to 255. The permanent
`test_stale_cleanup_never_follows_control_socket_symlink` and
`test_stale_cleanup_never_stops_replaced_or_unrecorded_real_socket` preserve these
failures, including missing and malformed recovery records. The loader records
the owned socket's device/inode in a private ephemeral recovery file and keeps
the same expected identity in memory. Shutdown checks that identity instead of
trusting a mutable pathname. Normal and kept completion remove the marker;
validated stale cleanup skips unrecorded or changed sockets.

All ten focused SSH/configuration regressions passed after these fixes, and
strict metrics clippy passed. Successful child completion also requests native
operation shutdown before unmount/state locking through a separate flag; this
does not invent a termination signal. Its real-mounted stalled-read and busy
recovery evidence is tracked separately and requires exact-head FUSE CI.

## Pending requester signals during a stalled mounted fetch

Actual real-FUSE CI run
[38164674316](https://github.com/progbeat/git-on/actions/runs/38164674316)
at `fd7367e01a9413c39eb4bebed4dde2e70e8f0c94` reproduced a remaining shutdown
deadlock. The controlled server had received the lazy blob request, and SIGTERM
was delivered to the blocked foreground child group. The runner still failed
to finish within the permanent test's eight-second bound. The authentic
before-fix binary reproduced both stalled cases; successful leader completion
with an escaped reader passed after the separate operation-stop fix. The
foreground signal case required an additional fix because the child could not
exit until its blocked FUSE request received a reply. fuser 0.16 dispatches on
one synchronous session thread and does not implement FUSE_INTERRUPT.

Demand-capable callbacks now bind cancellation to their actual requesting task
with a held `/proc/TID/status` descriptor. Existing native-operation and cache
lock polls detect pending unblocked, nonignored INT/TERM/HUP/QUIT signals and
abort only that request. The typed interruption propagates as EINTR and does
not latch a remote failure or invent a command signal. Caught handlers can run
and retry; the child's actual outcome remains authoritative. Native groups get
a bounded SIGTERM cleanup interval, then SIGKILL before the unreaped leader is
reaped. An interrupted private fetch removes only a leftover `shallow.lock`.

Review also observed that a real stopped requester with the non-UTF-8 task name
`\xffrequester` had a pending unblocked SIGTERM, while decoding its entire proc
status as UTF-8 failed. The initial whole-record UTF-8 decoder would therefore
miss that cancellation. The retained tests use this real task name; mask parsing
now reads ASCII signal records from bytes. A constructed long Groups prefix
checks parsing beyond 16 KiB; the held descriptor is read to completion without
a length cutoff.

All four focused `git::tests` passed locally: native signal dispositions and
task identity, mask/encoding/length handling, active operation interruption
without global cancellation, and termination of SIGTERM-ignoring groups with
an early exiting leader.

Actual real-FUSE CI run
[38165994416](https://github.com/progbeat/git-on/actions/runs/38165994416)
at `d040aae7470f0c6d0774fd8ffaebb07fc20972b7` then passed every permanent
before/after cancellation replay: foreground SIGTERM, successful completion
with an escaped reader, Python SIGINT, caught SIGTERM exiting successfully and
caught SIGTERM retrying the same object after its handler ran. The caught cache
fault also reproduced the old status 0 and required current status 125. The run
failed separately when a test helper trimmed a binary OpenPGP export; these
successful mounted cases do not imply that all checks passed at that head.

## Binary output preserved by the transport fixture helper

The same run `38165994416` failed
`test_openpgp_invalid_and_unknown_trust_match_native_git` when GPG imported its
disposable exported public key with an "Invalid packet" error. Review found the
shared subprocess helper returned `stdout.strip()`, which can truncate valid
binary signature bytes whose values happen to be whitespace. The production
loader and native Git verification were not responsible for that corruption.

The permanent `test_review_fixture_binary_stdout_is_preserved` failed before
the fix: all twelve leading/trailing whitespace bytes around NUL/non-UTF-8 data
were removed. Independent review reproduced the same loss with a raw 21-byte
payload reduced to nine bytes. The helper now returns stdout unchanged; OID/ref
text and the Git executable-directory path normalize text explicitly. The local
Git blob oracle also compares unchanged bytes, including the final newline.
GPG cleanup is registered when each disposable home is created, covering failed
imports as well as normal completion.

Independent review audited every shared-helper caller and the other binary
oracle paths: signed commit `cat-file` output and smart-HTTP CGI responses already
retain raw stdout, and the benchmark payload oracle explicitly disables text
trimming. The permanent binary-helper test passed after the fix. The native
OpenPGP unknown-trust/invalid-signature comparison and HTTPS cold/warm local
equivalence tests also passed together (two tests, 1.258 seconds). The complete
local remote suite then passed all 21 executable cases with eight explicit
FUSE/million-selector skips, and all ten SSH/configuration regressions passed.
The next exact-head CI run must still pass every required check.

## Explicit remote refs, byte names and displayed verification options

The follow-up review at `77428030eed52d440b682eb55e9249f63a5d5e8b`
identified three reproducible selection/presentation defects. Real disposable
Git upload-pack advertisements confirmed that HEAD and `refs/heads/main` remain
valid explicit selections even with branches/tags named HEAD or full-ref-looking
short names such as `refs/heads/refs/heads/main`. Native Git fetched their literal
targets, while the source probe rejected both as ambiguous. The retained HTTPS
`test_review_explicit_remote_refs_do_not_expand_as_short_names` reproduced both
failures against distinct colliding trees before the fix, then pinned and read
the intended original tree afterward. Short branch/tag ambiguity is preserved.

Native Git also accepted a valid unrelated `refs/heads/unrelated-\xff` byte name
and advertised it unchanged. The old resolver decoded the whole advertisement
and rejected valid ASCII HEAD/full-ref selection as "remote refs are not UTF-8".
`test_review_unrelated_non_utf8_remote_ref_preserves_ascii_selection` retains
that native byte-ref creation, checks the raw advertisement and reproduces both
ASCII-selection failures. Resolution now compares line/tab/ref names as bytes
and decodes an object ID only for a matched record. Both selected trees and blob
contents passed after the fix.

The parser already accepted `--verify`; the defect was its help line listing
only keep flags after REV. The existing CLI help acceptance
assertion failed when required to show `REV [--keep|-k|--verify]`, then passed
after including verification in the displayed post-REV options. Native verification
behavior and command-tail parsing remain unchanged. All three focused loader
regressions and the help acceptance passed after the fixes.

## Local unverified execution retains the 1.0.0 runtime contract

Follow-up review identified that local execution without `--verify` now called
native Git configuration capture and revision resolution unconditionally.
Independent comparison of unchanged snapshot modules from authentic master
`b411e5d00018be66475116c3373eff9e21d2a69e` and feature head
`77428030eed52d440b682eb55e9249f63a5d5e8b` reproduced the regression using the
same compiled libgit2/dependencies and disposable repositories. With PATH set
to an empty directory, master loaded and read `HEAD`, `HEAD:deep`, `HEAD^`, an
annotated tag and a 70-tag chain; the feature failed each selection with
"read effective Git configuration: start native Git operation: No such file
or directory". Replacement refs pointing HEAD at a different tree did not
redirect the master's exact object reads.

A second comparison with native Git available exposed an additional legacy
compatibility change: master read the 70-tag chain successfully, while the
feature failed with "annotated tag chain exceeds 64 objects". The new bound
belongs to remote/verified resolution, not existing unverified local execution.
The project's release policy requires a minor release to preserve its documented
environment contract; the 1.0.0 runtime requirements did not include native Git
for direct local binary invocation.

Local unverified selection now uses the original libgit2 discovery,
`revparse_single` and `peel_to_tree` path before the shared exact-tree pin/setup.
Remote and verified resolution retain native Git and exact commit/tag signature
binding. Independent replay against the fixed source passed all five former
failures with no Git on PATH, including replacements and the 70-tag chain.
The permanent `test_review_local_unverified_loader_needs_no_native_git` covers
the Git-free loader, selectors, ancestry, replacements and the 70-tag chain,
and passed independently after the fix. The mounted
`test_review_local_unverified_run_without_native_git` runs the actual binary
with only FUSE helpers on PATH and an absolute Python command. Mounted evidence
still requires the next exact-head CI run. Runtime documentation now scopes the
native Git dependency to remote execution, verification and the `git on`
external-command invocation, preserving the additive 1.1.0 release classification.

## Advertised execution syntax puts options after REV

The requested presentation change uses `git on REV [OPTIONS] -- COMMAND [ARGS...]`
and `git on REPOSITORY REV [OPTIONS] -- COMMAND [ARGS...]` throughout help and
user-facing documentation. Keep and verification examples use that order;
existing compatibility parsing and its tests are retained without changing
accepted argv or forwarding behavior.

The revised `CliTests.test_help_and_version` failed against
`f4a97e9c007d3d9c03dbfe6a7da5da0d271b8833` because help still advertised an
option slot ahead of the operands. It now requires both canonical forms and
the instruction that options follow REV. The same acceptance test executes
`git on -h` through native Git, covering the working help spelling used in the
touched README/usage examples. Git's long-help dispatch looks for a manpage
instead of calling the external binary; no manpage or dispatch behavior is
introduced by this presentation change.

The focused help acceptance and all eight existing CLI parser tests passed
after the change, including compatibility argument parsing and literal command
tails. Required real-FUSE and distribution checks still run at the final head.

## Clap CLI migration preserves raw argv and control modes

The CLI uses clap 4.6.7's builder API with `std`, `help`, `usage` and
`error-context`, and no derive feature. One argument definition produces
structured help and contextual errors. The advertised form is
`git on [REPOSITORY] REV [OPTIONS] -- COMMAND [ARGS...]`; keep/verification
examples continue to put options after REV. Git's manpage interception is
separate, so help examples use `git on -h` or direct `git-on --help`.

Execution splits the first literal `--` from raw OS arguments before parsing.
Clap sees only the prefix. Command names and later arguments, including empty
strings, non-UTF-8 bytes, help flags and later separators, remain untouched.
Existing first-token help/version precedence, version bytes, literal cleanup
paths, repeated flags and accepted compatibility forms are preserved.

Independent comparison against the unchanged parser at
`9b4f54951a2998ca33dc69aa6db4284aeb7b40db` matched 187,505 cases, including
interspersed flags, two-operand selection and byte-exact command tails. Review
also reproduced two native clap defaults that need compatibility guards:
short-option bundles were accepted, and an Append positional accepted more than
two total values when flags interrupted its occurrences. Existing exact-token
regressions and the retained `interspersed_flags_do_not_relax_total_operand_limit`
plus black-box no-runtime cases require usage status 2 for those inputs. The
guards derive flag spellings from the argument definition and validate the
complete operand count. Eleven parser tests and eight CLI acceptance cases
passed on Rust 1.89, including actual `git on -h` dispatch and raw cleanup paths.

The actual locked graph passed normal/metrics Rust tests (86 each) and strict
clippy on Rust 1.89. CI also checks every locked target with test metrics at that
MSRV. The only four added packages are clap/clap_builder 4.6.7, clap_lex 1.1.1
and anstyle 1.0.14. No additional shared runtime library is introduced.

Release sizes use the same checkout path, Rust 1.89.0, default release profile,
target directory and strip command (`strip --strip-all`). The before revision
is the exact pre-clap head above; only the CLI/dependency migration changes the
after build.

| Measurement | Before | After | Increase |
| --- | ---: | ---: | ---: |
| Release binary, unstripped | 2,755,920 bytes | 3,328,000 bytes | 572,080 bytes (20.8%) |
| Same binary, symbols stripped | 2,307,952 bytes | 2,783,088 bytes | 475,136 bytes (20.6%) |

Independent CLI review found no remaining behavior defect. Actual FUSE,
performance and installed-distribution evidence still requires all checks at
the final published head.

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

## Earlier CLI proposal (superseded before release)

The first unreleased CLI proposal reserved an initial post-revision `--` boundary
and added `-k`. It was superseded by the mandatory separator contract below.
In that proposal, ordinary separator-free calls remained literal and the
newly reserved dash-command forms required an explicit boundary. It also
supported prefix revision escapes. Those forms were changed before release
when the mandatory separator was approved. The earlier result below records
that proposal alone, not the current syntax.

Red-before was run against the published v0.1.1 Linux x86_64 archive after
checking both SHA256SUMS entries and `release.json`'s source commit
`257d50f10aa461bc7c9e57ae97aaaf8604e9f6da`. In an isolated Linux container,
`CliTests.test_explicit_option_errors_never_resolve_tree_or_execute` produced
13 failing subcases: unknown explicit options, empty revision/command names,
and missing commands after the new separator returned 125 rather than 2.
The old binary tried revision resolution or mounting; this container had no
FUSE device, so this red result makes no real-mount claim.

At the recorded revision, six parser unit tests passed, including both keep
spellings/positions, repetition, first-boundary handling, legacy literal argv,
revision escapes, and empty/non-UTF-8 arguments. The retained black-box regression checks usage 2
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

## Mandatory command separator

The final 1.0.0 contract requires the first `--` before every executed command.
Only one local revision and exact `--keep`/`-k` flags are accepted before it;
all subsequent argv is literal. This supersedes the unreleased proposal above
within the same major bump. Existing help/version/cleanup modes remain separate.

The permanent parser test
`cli::tests::mandatory_separator_rejects_old_direct_command_syntax` was run
against the exact parser from `5975d54ad6b9799a23a464c1d6b1c03b7ca948ce` in an
isolated Rust harness. A wrapper converted its `Option` to `Result` without
changing acceptance. It failed on `["HEAD", "cmd"]`, which the old parser
accepted. The same test passes with the mandatory boundary; all seven new
parser tests pass under strict standalone clippy. This establishes parser
red-before/green-after, not real-FUSE execution.

`CliTests.test_direct_command_syntax_is_rejected_before_execution` permanently
checks the public binary's usage status and missing-separator diagnostic, a
nonexecuted sentinel, and absence of mounts, backing directories and Git pins.
The malformed call with multiple operands and an unknown option asserts clear
usage failure without depending on which invalid token is diagnosed first.
Independent review caught an earlier over-specific assertion that expected the
operand-count error although `-c` was encountered as an unknown runner option.
The test now checks the agreed observable contract; dedicated parser cases
still verify isolated operand-count and unknown-option failures.

Real-FUSE tests preserve exact binary streams and argv, both keep spellings,
compatibility argument parsing, retained subtree writes and pin/unmount cleanup. Both
stress and performance launchers use the mandatory separator. Green-after is
confirmed by [CI run 38151612805](https://github.com/progbeat/git-on/actions/runs/38151612805)
at `01248000f6b38211f67222caf88e69b5fea8e720`: all 65 real-FUSE/uv integration
tests pass without skips, and the direct-command rejection regression passes
in the debug and installed binaries. All five mandatory suites pass, including
strict Rust/clippy, FUSE protocol/inventory checks, stress, million-file demand
gates, installer checks and packaged real-FUSE acceptance. Later commits require
fresh exact-head CI. Native macOS checks do not establish FUSE support, and the
benchmark timeout self-test also requires Linux `/proc`.

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


## SSH control precedence, direct commands and terminal interruption

The mounted shutdown regression replays the authentic feature checkpoint
`92377e27e32ac424d688ae8bcf0fbe6be0081c0c` in the feature PR's real-FUSE CI.
`test_review_real_mount_stalled_lazy_fetch_shutdown` holds an actual smart-HTTPS
blob request while either signalling the foreground command or letting its
leader exit with an escaped reader. It verifies that the old runner remains
blocked, then requires bounded completion, correct status and recovery behavior
from the fixed runner. The escaped reader retains its cwd so busy cleanup must
preserve the run until it exits. Historical replay is conditional after squash
merges; the permanent fixed-behavior cases always run with required real FUSE.
Local loader/protocol results do not establish these mounted cases.

`test_review_real_mount_caught_cache_fault_forces_runner_failure` additionally
injects a real cache error after mounting, catches its EIO in a successful
command, and compares the old runner's status 0 with the required fixed status
125 and diagnostic. Exact-head CI supplies execution evidence for both cases.

Independent review in the fresh runtime reproduced an unused-environment
precedence defect: native `git ls-remote` succeeded with a valid
`GIT_SSH_COMMAND` and `GIT_SSH=/does/not/exist`, while the restored source probe
exited 1 with "GIT_SSH wrappers are unsupported". A PTY running the actual CLI
with a disposable stalled transport exited 125 on terminal Ctrl-C and printed
an empty authentication failure, rather than propagating SIGINT. The permanent
`SshRegressionTests.test_unused_git_ssh_respects_command_and_configuration_precedence`
and `test_terminal_interrupt_during_authentication_propagates_sigint` preserve
those same focused reproductions and pass after respecting native command
precedence and recording a foreground native child's terminating signal.

The previously red
`RemoteTests.test_review_connection_loss_cannot_reconnect_with_explicit_proxy_option`
now passes. Owned first-value options, a final short `-S` before Git's host, and
neutralization of short `-M` prevent user control flags from replacing the owned
socket/master. Lazy commands use OpenSSH `-O proxy`, which requires that socket,
instead of a competing ProxyCommand guard. Direct native transport experiments
showed short `-M` could create a second authenticated connection even with
`-O proxy`; the permanent short-control and lost-master tests count one
connection and one encrypted-identity prompt. The caller's one-second
ControlPersist setting cannot expire the run's owned master. Short `-J none`
continues to work. A transparent `GIT_SSH` executable with spaces in its path
preserves argv and the encrypted-identity one-prompt contract.

`python3 tests/ssh_regressions.py` passed all seven focused cases. The existing
known-red ProxyCommand case, encrypted-identity, unlocked-agent and external
SIGTERM transport tests also passed (four cases). These are real disposable
OpenSSH and PTY checks without FUSE. Busy/stale mounted cleanup and the final
independent review still require completion; no actual mount or exact-head CI
result is claimed here. Direct shell prefixes/compounds and non-OpenSSH variants
are diagnosed precisely rather than claimed supported.

## Cache restoration errors must survive a caught filesystem failure

Fresh independent cross-review reproduced a demanded cache error that escaped
runner failure reporting. `Remote::ensure` returned errors from cache restoration
before its fetch-only failure latch. With a disposable filtered server, the
permanent `review_cache_restore_failure_is_latched` inserts an unusable cache
entry after startup and demands an uncached subtree. Before the fix, the
operation failed while `Snapshot::remote_failure()` remained `None`; the focused
Rust regression exited 101 with "demanded cache failure was not latched for
runner failure". The authentic before-fix feature checkpoint is
`92377e27e32ac424d688ae8bcf0fbe6be0081c0c`.

`Remote::ensure` now latches repository, ODB, cache and fetch failures alike,
reports the first failure, and retains that original diagnostic across later
demands. Removing the unusable cache entry cannot trigger another transport
attempt after failure. The isolated regression avoids changing the process-wide
cache environment of concurrent tests. `cargo test --locked --test
remote_source` passed all seven cases after the fix. This loader regression
verifies the flag consumed by runner completion; a caught EIO on an actual
mount still needs the required real-FUSE acceptance run.
