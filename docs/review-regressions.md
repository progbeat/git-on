# Review regression evidence

## Native Git operand compatibility (1.2.0 bug fixes)

Repository and revision arguments were always intended to use Git's syntax.
These corrections restore that contract; the release's minor version increase
is for the additional write policies. They do not introduce a new grammar or
require a major version increase.

The authentic pre-fix binaries from `85c1b15` were copied before editing the
runtime sources. Five focused cases failed in 14 subcases against those
binaries, including native ref spellings, direct local repository paths,
branch/tag precedence and literal destination refspecs. The same permanent
cases pass after delegation to native Git. Each fixture compares its selection
or error with Git itself rather than prescribing a second interpretation.

| Reproduced failure | Permanent regression | Result after fix |
| --- | --- | --- |
| Native ref forms such as `heads/main`, `tags/v1`, `+main` and destination refspecs were rejected or reinterpreted | `test_fetch_accepts_native_short_ref_names_and_refspecs`, `test_fetch_colon_is_a_literal_refspec_and_does_not_touch_caller_refs` | Native fetch selects the object and updates only private destination refs |
| Local and non-UTF-8 source paths were rejected; a branch/tag collision used custom ambiguity rules | `test_fetch_accepts_relative_absolute_and_non_utf8_repository_paths`, `test_fetch_branch_tag_collision_follows_native_precedence` | Exact operand bytes and native precedence are preserved |
| Invalid or non-UTF-8 revision operands never reached Git | `test_fetch_invalid_revision_reports_native_git_error`, `test_fetch_non_utf8_ref_is_passed_to_git_unchanged`, `test_native_fetch_receives_the_original_source_and_refspec` | Git receives unchanged operands and supplies the diagnostic |
| A cold fetch into a destination ref made later tree demand negotiate from an incomplete private commit | `test_fetch_accepts_native_short_ref_names_and_refspecs`, `test_two_operands_fetch_literal_refspec_from_local_paths_into_a_private_snapshot` | Demand fetching obtains the exact missing object without advertising incomplete history |
| Native resolution found objects in an external ODB, but subsequent reads or verification lost that environment | `test_local_native_object_database_environment_is_used_for_exact_reads`, `test_local_verification_keeps_native_object_database_environment` | Reads and signature verification use the Git-selected object locations |
| Namespace/common-directory environments left temporary pins after completion or stale recovery | `test_local_namespace_environment_does_not_leave_snapshot_pins`, `test_local_common_directory_reads_and_releases_pins_in_the_native_repository` | Pins are removed from the actual common directory on completion and after SIGKILL recovery |
| C-quoted alternate paths containing a colon or non-UTF-8 bytes resolved in Git but failed during snapshot reads | `test_local_c_quoted_alternate_with_colon_and_non_utf8_path_matches_native_git` | Private aliases preserve native object access without scanning or copying the ODB |
| Copilot found relative alternates used the invocation cwd after native Git had changed repository context; relative primary object directories had the same defect | `test_local_relative_object_environments_follow_native_worktree_setup`, `test_local_relative_alternates_keep_native_bare_invocation_context` | Four native-valid subcases failed against the preserved binary; all eight worktree/bare/explicit-context cases pass with native object-location queries and real FUSE |

Local runtime regressions were reproduced before their individual fixes, then
rerun against immutable copies of the final executable and source probe. Those
copies also passed 72 real-FUSE integration/uv tests and all nine write-policy
tests. The full remote suite passed through real HTTPS/OpenSSH and FUSE,
including the cold/warm million-file demand gate; the later object-environment
fixes concern local repositories. Native fetch hooks are exercised explicitly
by `test_native_fetch_reference_hook_sees_private_refs_and_repository`.

The first exact-head CI run (`fb2556d`) reproduced a protocol-fixture defect:
`protocol_all_copy_up_setattr_links_rename_and_whiteouts` failed with EACCES and
`protocol_all_setattr_on_unlinked_original_read_handle` with EPERM. The fixture
sent UID/GID 1000 while tracked entries belonged to the actual runner process.
It now defaults to that process's effective IDs. Credential tests still use
distinct owner, stranger, group and root identities and retain their rejection
assertions. Both focused cases and all 17 protocol cases pass locally in normal
and `test-metrics` builds; the subsequent CI run checks the corrected fixture
under the runner's different UID. Production permission checks are unchanged.

## Four write policies and release classification (1.2.0)

The base for this change is current master `c42f818f1b4e2ccf4d940c2a77e98e2ad1eb931b`
(1.1.0). A separate detached checkout and target directory replayed its actual
binary: the new policy argument failed with usage status 2, and the focused
missing-command diagnostic assertion failed against its generic `spawn command`
message. Both tests pass with the new implementation. Installer and release
regressions were also run before replacing their implementations.

| Reproduced failure | Permanent regression | Result after fix |
| --- | --- | --- |
| Installer refused owned mode 0775 without path/mode/owner or conditional advice | `test_owned_group_writable_directory_has_precise_conditional_advice`, `test_other_writable_directory_is_distinguished_from_group_write`, `test_foreign_owner_never_gets_permission_change_advice` | Precise reasons, safe single-directory advice only where appropriate, permissions unchanged |
| Owned mode 0555 failed staging with only a random basename and EACCES | `test_owner_write_missing_names_directory_mode_and_safe_alternative` | Full directory context and an accessible `--bin-dir` alternative |
| Old Git 2.43.0 and missing Git diagnostics omitted detected version or next steps | `RemoteDiagnosticsTests` in `tests/remote.py` | Requirement stays 2.45.1+, with detected version/PATH guidance |
| GitHub SSH publickey failure offered no public HTTPS alternative | `RemoteDiagnosticsTests` | Conditional HTTPS hint; no automatic retry or weakened authentication |
| Version preflight before configuration capture blocked on a real Git 2.55 global-config FIFO | `SshRegressionTests.test_native_config_fifo_cancellation_stops_git_and_removes_state` | Restored configuration-first ordering; original lifecycle assertion passes |
| Docs/test-only changes required a binary release; nonrelease publication could inspect old assets | `tests/test_release_policy.py`, `tests/test_publish_release.py` | Retain version, emit skip decision, avoid artifact/API access |
| Copilot found nonrelease checks accepted changed, removed or newly added release notes despite the retention contract | `test_nonrelease_changes_must_retain_release_notes_bytes`, `test_nonrelease_cannot_add_previously_absent_release_notes`, `test_nonrelease_can_retain_absent_release_notes` | Four cases fail before the fix; all 32 release-policy and 12 publisher tests pass with baseline bytes and file presence enforced |
| Workflow-level RUSTFLAGS/defaults could change the binary without requiring a release | `test_inherited_workflow_rustflags_requires_bump`, `test_inherited_workflow_run_defaults_requires_bump` | Inherited build settings require a bump |

Two defects reproduced during development of `all` are separate from the
1.1.0 baseline, which had no `all` option. Renaming a listed source directory
unnecessarily demanded unvisited child headers (EIO with an unavailable child
blob). `all_directory_rename_never_demands_listed_child_headers` now proves
lazy remapping without that access. Changing metadata through a read descriptor
after unlinking a source file returned ENOENT; the focused real-mount
`WritePolicyTests.test_all_open_unlinked_source_allows_descriptor_metadata_changes`
failed before its fix and passes afterward. Unit and protocol regressions also
cover that anonymous copied-up inode.

`tests/write_policy.py --require-fuse` checks four policies, default compatibility,
tracked-parent protection, dirty host index/worktree selection, mutations, links,
metadata, kept artifacts and interrupt cleanup. Protocol tests independently
exercise fuser dispatch. Test results on one host do not replace exact-head CI
or verification on another required host. The SL recording provenance names
the actual recording host; it is not evidence of a recording on `brick`.

## Approved release LTO profile

The release profile now explicitly keeps optimization level 3 and enables fat
LTO with one codegen unit. No clap feature, panic strategy, source behavior,
runtime dependency, target CPU or lockfile change accompanies it. Rust crate
optimization does not imply cross-language LTO of bundled C libgit2.

A controlled comparison of the `d10b756` production source used Rust 1.89.0,
the same checkout/target paths, four build jobs, the locked offline graph, and
GNU strip 2.44. The target's release graph was cleaned between builds:

| Measurement | Previous release defaults | Approved profile |
| --- | ---: | ---: |
| Unstripped executable | 3,407,608 bytes | 2,783,136 bytes |
| `strip --strip-all` copy | 2,840,432 bytes | 2,443,096 bytes |
| Clean build wall time | 50.8 seconds | 61.4 seconds |

The stripped reduction is 397,336 bytes (14.0%); the unstripped reduction is
18.3%. Stripping is a measurement operation: distribution stripping policy is
unchanged. Help/version output matched byte-for-byte and shared runtime
requirements stayed the same. A single paired build shows optimization/link
cost, with dependency download time excluded and cache/order effects remaining;
it is not a stable build-time guarantee. The reported largest child-process RSS
was about 409 MiB in each build, not aggregate concurrent build memory.

Seven interleaved production Snapshot trials each performed 500,000 warmed
lookups and 500,000 64-byte reads, with identical checksums. Median lookup time
was 176.1 → 164.7 ms (6.5% lower); read time was 3951.5 → 3989.8 ms (1.0% higher).
These narrow in-process measurements exclude FUSE/network work and remain
subject to scheduler/filesystem noise; they establish no general speed guarantee.

The actual Rust 1.89 candidate passed all eight CLI cases, 14 disposable SSH
regressions and 27 loader/remote cases. Eleven local remote cases were explicitly
skipped because they require actual FUSE or the separate million-file selector.
CI now links the locked release executable on Rust 1.89 and exercises the
optimized profile through full real-FUSE integration, remote lifecycle, SSH,
stress, million-file demand and packaged-binary gates. Final execution evidence
belongs to the PR's exact-head checks; local skips are not FUSE evidence.

The first optimized CI run exposed a harness defect with relative artifact
arguments: the encrypted-key PTY child resolved the probe after changing to its
fixture directory. Failed exec then reentered the inherited unittest loop,
producing nested errors instead of an SSH prompt. The same ordering existed in
the Ctrl-C PTY runner. The encrypted-key test failed with a relative probe before
the fix; both PTY cases pass afterward. Artifact paths are now resolved once
before any fixture chdir. The permanent
`test_fixture_exec_failure_exits_without_reentering_test_runner` reproduced child
status 1 and a nested unittest traceback before handling failed exec; it now
requires status 127, a bounded diagnostic and no test-runner reentry. This changes
the harness, not the released executable or measured size.

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

At this checkpoint binary-size experiments remained outside the PR. The
subsequently approved release profile is recorded above; clap features remain
unchanged.

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

## PR 5: SSH boundaries, ignore-acquisition errno, and authentication job control

The authentic before-fix head for these three defects is
`e42774399dba61d02c80235ef54bf61c3abd9e63`. Its commit/tree/blob hashes and
ancestry were restored and verified before building an unchanged executable
and metrics source probe. The permanent CI replay builds that exact revision
in a detached worktree, copies both artifacts, then rebuilds the reviewed head.

`SshRegressionTests.test_user_control_path_expansion_cannot_hide_master_options_after_connection_loss`
expands `-oControlPath="$GIT_ON_SSH_CONTROL_PATH"` in the caller's direct SSH
command before `-M` or `-vvM`, authenticates against a disposable OpenSSH server,
and explicitly closes the owned master before demanding an uncached blob.
The unchanged source probe failed the unencrypted cases with a replacement TCP
connection (observed connection count 4 to 6). Encrypted-key cases additionally
exposed replacement authentication during initial object acquisition. The fixed
probe passes all four cases, counts one accepted authentication and, for an
encrypted key, one ASKPASS invocation, and fails the demand without a new
connection. The selected command is expanded once; Git's outer argv count
defines the structural boundary instead of a caller-visible value. The
`--before-boundary-probe` replay requires the specific lost-master reconnect
observation, not an arbitrary setup/authentication error.

Independent review also found that a caller-supplied positional SSH host and
quoted `git-upload-pack REPOSITORY #` command turned the injected control options
into remote command text. The initial structural-boundary fix still reproduced
this: after forcibly closing the master, a lazy read succeeded with accepted
authentications increasing from 1 to 3. The permanent
`test_user_ssh_host_and_remote_command_cannot_swallow_owned_control_options`
replays that reconnect on the authentic old head (TCP connections 2 to 4), then
requires rejection before authentication. The wrapper rejects unconsumed
positional operands and option terminators, including `-M-`/`--M` that would
normalize to `--` when removing master flags.
`test_ssh_tag_option_values_resembling_control_flags_remain_literal` uses an
OpenSSH `Match tagged` identity to prove `-P -M` and `-P -O` preserve their tag
values. Both subcases fail on the authentic old probe: the stripped `-M` tag
causes public-key denial and `-O` is incorrectly rejected as a control command.
Both pass after recognizing `-P` as an option consuming one operand. This
additional test explicitly skips only on OpenSSH versions without tag support.

`filesystem::tests::missing_ignore_object_is_io_error_for_file_and_directory_creation`
removes a real loose `.gitignore` object before policy demand. Inserting only
this regression into the unchanged before-fix source produces
`[Err(30), Err(30)]` instead of `[Err(5), Err(5)]`; the corrected source passes
both file and directory creation checks without inserting namespace or backing
entries. Policy loader errors now use `snapshot_errno`, retaining EINTR for
request interruption and EROFS for an ordinary healthy-policy denial.
`RemoteTests.test_review_real_mount_ignore_acquisition_faults_report_eio`
requires actual FUSE and repeats create/mkdir with an exact remote ignore-object
403 response and a real invalid cache entry. `--before-policy-binary` checks
the original EROFS result and the corrected EIO result, failure diagnostics,
runner status 125 and absence of a created path.
`test_review_real_mount_interrupted_ignore_acquisition_preserves_eintr` stalls
the ignore request, signals its requesting task, observes exact libc syscall
EINTR before releasing the server, and verifies its handler and successful
retry. EINTR preservation is expected on both revisions. These mount-dependent
tests are explicitly skipped locally without `/dev/fuse`; those skips are not
mount evidence and the required real-FUSE CI job must supply it.

`tests/native_job_control.py` uses an interactive Bash in a Linux PTY and an
actual encrypted-key OpenSSH passphrase reader. On the unchanged executable,
Ctrl-Z stops native Git while git-on remains running, keeps the native group
foregrounded and never returns the shell prompt; the focused test fails at that
prompt assertion. The corrected executable passes `fg`, repeated stop/resume,
`bg` followed by terminal-input stop and `fg`, and Ctrl-C after resume. It checks
process/session/foreground groups, native unsigned-commit verification failure
before command execution, final status and process/socket/run cleanup. Native
Git stops are consumed separately with `waitid(WSTOPPED|WNOHANG)`, leaving exit
status ownership with `Child::try_wait`. `--before-binary` requires the exact
old stopped-Git/running-runner/no-prompt state before running the green suite.
These PTY checks do not need FUSE and do not stand in for real mounts.

The first full CI replay at `7f8483516165ff3a412560843ea2d4a3e2b8eee5`
([run 38214579320](https://github.com/progbeat/git-on/actions/runs/38214579320))
exposed an artifact-isolation defect in the replay recipe. Historical worktrees
overwrote the same top-level `target/release/git-on`; after compiling e427,
Cargo marked the reviewed head fresh in 0.05 seconds. The four retained
create/mkdir transport/cache assertions then observed EROFS from that old
executable instead of the required EIO. All four baseline EROFS observations
and both old/new EINTR retries were recorded, but this failed run cannot establish
the fixed behavior. A local Rust 1.89 replay confirmed the same contamination:
the historical build took 17.24 seconds, the subsequent reviewed build took
0.05 seconds, and its executable hash still matched the historical artifact
instead of the separately saved correct executable. Historical artifacts now
build and copy from a separate
`/tmp/git-on-historical-target`; the reviewed head keeps its own target directory.
The same real-FUSE assertions remain required in the replacement exact-head CI
run. Other green jobs in the failed run do not replace that acceptance.

## Native Git trust policy is the verification acceptance policy

The owner reconciled issue #4 with native Git policy delegation after review
discussion `r4245809531`. This is acceptance clarification and test hardening,
not a discovered runtime trust-policy defect. git-on intentionally adds no
signer allowlist or trust threshold to native Git's result under the caller's
effective configuration. A source comment beside that decision records why.

A disposable public-only GnuPG keyring on Git 2.52.0 / GnuPG 2.4.7 produced
`GOODSIG` and `TRUST_UNDEFINED` for an actual signed commit and annotated tag.
Both `verify-commit` and `verify-tag` returned 0 by default and with
`gpg.minTrustLevel=undefined`, and 1 with `marginal` or `fully`. This confirms
the configured threshold applies to those operations; it is not inferred from
Git's different default for signature verification during merges. See
[Git's configuration documentation](https://git-scm.com/docs/git-config#Documentation/git-config.txt-gpgminTrustLevel).

`test_openpgp_invalid_and_unknown_trust_match_native_git` now checks native
default acceptance of both commit and tag while retaining invalid-signature
rejection. `test_openpgp_configured_trust_rejection_prevents_local_and_remote_execution`
requires native rejection of the same good, unknown-trust signatures with
`gpg.minTrustLevel=fully`. All eight local/remote, commit/tag, repository/`git -c`
cases must return runner status 125 with the native verification-failure
diagnostic, no sentinel execution and no retained run state. The inherited
`git -c` case overrides a permissive repository setting and exercises effective
configuration replay into the private remote repository. These focused cases
passed against the unchanged `0cbbcf3d9ded0af5d9394e80caaad7e08a188662` runtime;
no artificial red-before runtime result is claimed.

`test_real_mount_openpgp_default_policy_executes_unknown_trust_commit_and_tag`
additionally requires actual command execution and expected pinned file bytes
for both default-accepted object kinds with local and remote operands. It is
explicitly skipped without required FUSE and must pass in exact-head real-FUSE
CI. Local source-probe acceptance and rejected-command checks are distinct from
that mounted evidence.

## SSH shutdown must not execute mutable client contents

Review `r4246168532` found that the client record at authentic head
`87a11d5bbc85b9b8b46a55d054a0e1a6538d5203` checked only metadata. An equal-length
in-place rewrite followed by restoration of mtime preserved device, inode,
length and timestamps, yet normal shutdown executed the changed script. The
standalone replay observed its harmless sentinel, runner/probe failure and a
still-live owned master, pin and run. The permanent
`test_equal_length_restored_mtime_client_change_retains_live_master_pin_and_run`
reproduces execution of that sentinel on both normal completion and stale
recovery before requiring the fixed behavior: no sentinel, a visible changed-
client failure, retained resources and successful cleanup after byte restoration.

`test_shutdown_never_executes_selected_path_after_validation` isolates the
pathname check/use race with a Linux/glibc `execvp`/`posix_spawn` interposer. The
fixture arms only after actual SSH startup, writes the same harmless equal-length
script immediately at the old shutdown spawn boundary and restores mtime.
The authentic old runtime executed the sentinel. Fixed shutdown does not reach
the armed spawn, leaves the selected file unchanged and closes the actual master
without running any selected-client bytes. Setup errors are not accepted as red
evidence. CI builds the authentic old executable and probe in the separate
historical target, copies both and replays all three observed failures.

Client contents now have a streaming SHA-256 fingerprint. File metadata and
contents are checked from one no-follow fd; this detects restored-mtime edits
but is deliberately not followed by executable-path or mutable-fd execution.
Instead, the runner sends OpenSSH's documented mux-v4 termination request over
the identity-checked owned Unix socket. This avoids script-interpreter/fd
execution pitfalls and introduces no shutdown child, new authentication or TCP
connection. Clients must be readable executable files. An older metadata-only
client record cannot validate contents, so a live master must first be closed
with a trusted client; its already-dead owned endpoint remains cleanable.

The permanent outside-PATH ELF and transparent-script tests still close real
masters on completion/recovery. Invalid or missing fingerprints retain state.
`mux_shutdown_rejects_invalid_greetings_before_termination`,
`mux_shutdown_rejects_nonmatching_acknowledgements` and
`mux_shutdown_deadline_cannot_be_extended_by_partial_packets` guard packet bounds,
protocol/reply checks and the shared deadline. Socket-pair results are not real
SSH or mount evidence; real-FUSE, full lifecycle and unchanged earlier review
regressions still require successful exact-head CI.

During parallel local validation the existing stalled-transport test once read
a live `/proc` state immediately after the Git leader was reaped; its isolated
replay passed. Group SIGKILL delivery to another member is asynchronous. That
test now opens a pidfd before cancellation and requires its bounded death,
without confusing an outstanding kill with a survivor or relying on container
PID 1 to reap zombies. This is an observed test-timing correction, not a claimed
new transport cancellation defect or a fabricated deterministic red schedule.

## Recording artifacts and CLI option placement (Copilot follow-up)

The authentic recording helper at `8660e5e2` protected only `demo.gif` and
`demo.json`. Five permanent destination tests failed before its fix: opening
`vhs.log` truncated both an existing log and an unrelated file reached through
a log symlink, while directories containing a video, dangling artifact symlink
or earlier capture still reached the mocked VHS launch and gained a new log.
These reproductions use real temporary files with mocked dependencies and VHS;
they do not claim a new real recording or FUSE execution.

All six `tests/test_record_demo.py` tests pass after rejecting a nonempty output
directory before dependency, FUSE or executable probes. Existing files, directory
contents and symlink targets remain unchanged; new and empty directories still
reach normal dependency checks. The default output is now the disposable
`target/sl-recording` directory. CI runs this permanent suite. Local red/green
logs are retained as `target/git-operands-{red,green}/copilot-recording-destination.txt`.

The CLI parser already accepted runner options before, between and after Git
operands, but help and normative documentation said they must follow `REV`.
The strengthened `CliTests.test_help_and_version` failed against the authentic
release executable with SHA-256 `1f96f5445474149ca71d554c63a6d1388956cd0f2fb2c3427c73fcc48e22f996`.
After correcting the help and documentation, all 11 `CliTests` and all 12 Rust
`cli::tests` pass; parser behavior and operand grammar are unchanged. Evidence
is in `target/git-operands-{red,green}/copilot-option-placement.log` and
`target/git-operands-green/copilot-cli-unit.log`.

## Installer PATH precedence (Copilot follow-up)

At authentic head `e94360c1a`, installation succeeded silently when PATH selected
an earlier executable named `git-on`. The preserved installer has SHA-256
`a767f86485298f686a2b70be25257703f03fa5558fb54406f71a324ff6f63c22`.
`test_success_identifies_an_earlier_executable_without_running_it` failed against
that script before the correction. It now requires the actual shadowing path
and a shell-quoted command that directly checks the newly installed binary.

The installer checks PATH selection with `which` and file identity with
`samefile`; it never executes the earlier binary or edits PATH or profiles.
`test_no_shadow_warning_when_installed_binary_is_first_on_path` and
`test_path_alias_of_installed_binary_is_not_a_shadow` cover the non-shadowing
cases, and the existing missing-directory guidance remains covered. All 33
installer tests pass after the correction; shell syntax and diff checks pass.
The authentic old script is `target/git-operands-red/installer-shadow-before.sh`,
with evidence in
`target/git-operands-{red,green}/copilot-installer-path-shadow.txt`.

## Hash-prefixed alternates: rejected Copilot finding

Copilot claimed that skipping an unquoted `#` component in
`GIT_ALTERNATE_OBJECT_DIRECTORIES` differed from native Git. The claim was
rejected after checking Git itself: host Git 2.55.0 ignores `#alternate-objects`
and `:#alternate-objects::`, but reads objects through `"#alternate-objects"`.
The minimum supported
[Git 2.45.1 source](https://github.com/git/git/blob/v2.45.1/object-file.c#L530-L579)
also treats a leading `#` as a comment before checking quoted paths, and
[`prepare_alt_odb`](https://github.com/git/git/blob/v2.45.1/object-file.c#L885-L894)
uses that parser for environment alternates with `PATH_SEP`.

`test_local_hash_prefixed_alternate_components_match_native_git` keeps the
commit, tree and blob exclusively in a real hash-prefixed alternate directory.
It compares native tree resolution and file reads with actual FUSE command
execution for five variants: unquoted, quoted, empty components around either
form, and a comment followed by a quoted path. Native-rejected variants require
runner status 125 and no command execution; accepted variants must read the
exact alternate bytes. Every variant leaves caller refs, object files and run
cleanup unchanged.

All five variants pass against the unchanged authentic release executable with
SHA-256 `7c5efe724cf608acbf0900566568e51d7d8056d8d704f9a71ed40c0d1936d68f`;
evidence is `target/git-operands-green/copilot-hash-alternate-native-oracle.txt`.
There is no runtime fix or authentic red-before defect to claim for this
finding. The original native oracle rejected the proposed acceptance behavior
before invoking the runner, so that failed experiment is not runtime regression
evidence.

## WebP converter output preservation (Copilot follow-up)

At authentic head `b00fb876`, the converter's `exists()` check missed a dangling
output symlink. With a real 8×6 two-frame GIF and Pillow 11.3.0/libwebp 1.5.0,
`test_dangling_output_symlink_does_not_create_its_target` failed because the
encoder created the link's previously absent target. A second controlled
interleaving created another writer's output during source decoding;
`test_output_created_during_source_decode_is_not_overwritten` failed because the
encoder replaced those bytes. The preserved original helper is
`target/git-operands-red/convert-demo-before.py`.

The converter now rejects existing entries, including dangling symlinks, and
passes an exclusively created `xb` file handle to the actual WebP encoder.
All six permanent `tests/test_convert_demo.py` tests pass: occupied regular,
symlink and directory outputs remain intact, competing output creation is
rejected, and the tiny real conversion preserves RGB pixels, dimensions,
70/130 ms frame timing and loop count. CI runs the suite in an isolated Python
environment with pinned Pillow 11.3.0; missing codec support fails the tests.
Evidence is retained in
`target/git-operands-{red,green}/copilot-converter-output.txt`.

## Output-directory substitution and failed encoding (Copilot follow-up)

The authentic helpers at `403a29da` reproduced two further failures. A
dependency callback replaced the initially empty recording destination with a
symlink after preflight; `test_destination_replaced_after_preflight_cannot_clobber_outside_log`
failed because the old helper truncated the outside directory's existing log.
A controlled encoder wrote partial bytes and then raised;
`test_encoder_failure_removes_partial_output_and_allows_retry` failed because
the final WebP filename remained occupied. Both are deterministic isolated-file
reproductions, rather than claims of another full terminal recording.

Recording now pins the accepted directory with a no-follow FD, writes only into
a private staging directory on that filesystem, and launches VHS with a pinned
staging cwd. Publication uses exclusive hard links into the held destination
FD and checks the pathname's identity. The ten recording tests preserve outside
files after early and late directory substitution, preserve a competing artifact
during publication, and retain every generated byte for new and existing empty
destinations. A failed VHS run reports that its staged artifacts were not
published instead of pointing to a log that cleanup removes.

Conversion encodes and validates in a private sibling staging directory before
exclusive hard-link publication. Failed encodes remove their owned temporary
files, leave the final name available for retry, and preserve another writer's
output. All eight conversion tests pass, including the real tiny-GIF pixel,
timing and loop checks. Authentic helper copies and red/green logs are under
`target/git-operands-{red,green}/copilot-record_demo-followup.txt` and
`target/git-operands-{red,green}/copilot-convert_demo-followup.txt`; the original
scripts are `record_demo-403-before.py` and `convert_demo-403-before.py` in the
red directory.

## Distinct UID/GID protocol coverage: rejected Copilot finding

Copilot's proposed substitution of `stranger_gid` for the existing UID field
was rejected against the locked
[fuser 0.16.0 attribute flags](https://github.com/cberner/fuser/blob/v0.16.0/src/ll/fuse_abi.rs#L115-L126)
and [setattr ABI](https://github.com/cberner/fuser/blob/v0.16.0/src/ll/fuse_abi.rs#L514-L551):
`FATTR_UID` has value 2 with the UID field at offset 76; `FATTR_GID` has value 4
with the GID field at offset 80.
The original `stranger_uid` assertion correctly tested the UID field.

`protocol_access_and_setattr_distinct_uid_and_gid` additionally exercises
UID/GID pairs `(1000, 1001)` and `(1, 0)`, preserving owner, group, stranger and
root access assertions. Owner/root UID and GID no-ops succeed; real changes
return EPERM and retain attributes. All 18 protocol cases pass in normal and
metrics builds with unchanged runtime source and executable. The proposed
incorrect substitution fails the new case; that red result rejects the proposed
change and is not evidence of an old runtime defect.

The proposed-change failure is recorded in
`target/git-operands-red/copilot-setattr-proposed-change/regression.log`.
Protocol, distinct-credential, strict-clippy, coverage and release-policy green
results are retained in `target/git-operands-green/copilot-setattr-*.txt`.

## Native Git dependency and release classification

Copilot's `39c11087` review correctly identified that the former direct local
Git-free path is removed. The native resolver requirement is intentional:
the requested compatibility contract makes Git itself the only authority for
repository and revision interpretation. Preserving a second resolver would
violate that requirement. The requested release classification explicitly
treats this restoration as a bug fix, including the local dependency change;
the additional write policies determine the single minor bump to 1.2.0.

The specification, release rules and migration notes now state this consequence
explicitly. `test_local_unverified_run_requires_native_git_for_revision_resolution`
already checks the required executable and diagnostic. This clarification does
not claim a new runtime defect or red-before/green-after behavior change.

## Multi-artifact recording publication rollback (Copilot follow-up)

At authentic head `39c11087`, four permanent recording tests failed. A collision
on the second artifact and a simulated disk-space error both left the first
published hard link behind. When another writer replaced the first link before
a later collision, the helper preserved that replacement but left a second
owned artifact behind. A later unsupported directory was rejected only after
an earlier regular file had already been published.

Publication now validates every staged entry before creating any destination
link. It records successfully created links and, on failure, rolls them back
through the held destination FD only when the current no-follow entry still
has the staged source's device and inode. Missing entries and another writer's
replacement are preserved; the competing name that caused a collision is never
owned by this publication. Staged source files remain unchanged.

All 14 `tests/test_record_demo.py` tests pass after the correction, including
four new controlled real-file regressions and the existing ten destination
checks. The unsupported-entry case requires zero link calls; the two-artifact
cases exercise a real first link before a second collision or injected ENOSPC;
the three-artifact replacement case preserves both competing files while
removing the remaining owned link. These fixtures do not run VHS or mount FUSE
and do not claim exhaustive concurrent interleaving coverage. Runtime source,
release binary and the published animation are unaffected by this helper fix.

The authentic helper has SHA-256
`8b67221857e2a63a7688cb29c401bd83d82bd6993f2f8f63aa2f3f5241c60ef2` and
is preserved at `target/git-operands-red/record_demo-39-before.py`. Red/green
results are in
`target/git-operands-{red,green}/copilot-recording-publication-rollback.txt`.

## Owned SSH socket disappearance during signal cleanup

The real-FUSE job for `39c11087` failed the SIGQUIT subcase of
`test_real_mount_all_handled_signals_close_owned_ssh_and_remove_state`: the
runner returned the expected signal status but retained its temporary directory.
The authentic release executable with SHA-256
`7c5efe724cf608acbf0900566568e51d7d8056d8d704f9a71ed40c0d1936d68f`
also reproduced that assertion locally on the fourth focused replay, for SIGINT.

An isolated copy of the same runtime, instrumented only to print its existing
cleanup error before reraising the signal, reproduced the failure on its fifth
replay. It reported `remove disconnected owned SSH control socket: No such file
or directory` and no remaining mount. OpenSSH had removed its own socket between
the runner's final ownership check and unlink. The original signal-specific
core-dump hypothesis therefore did not explain this failure.

`remove_disconnected_socket` now treats `NotFound` from that final unlink as an
already completed removal. Other errors continue to retain recovery state.
The permanent
`SshRegressionTests.test_owned_socket_removed_during_unlink_does_not_block_recovery`
uses a real closed Unix socket and valid private recovery state. A test-only
`LD_PRELOAD` fixture removes the socket during the runner's unlink call, after
its ownership check. This controlled interleaving failed against the authentic
executable with status 125 and ENOENT, then passed after the correction. A
separate injected EACCES case requires status 125 and preserves the socket and
recovery state; ordinary recovery succeeds after that injection is removed.

All 21 SSH regressions pass with the corrected metrics executable. Twelve
bounded real-FUSE replays additionally passed all four handled signals, for
48 signal completions; this is not exhaustive interleaving coverage. Formatting
and the FUSE inventory guard pass. The authentic executable is retained at
`target/cleanup-forensics-authentic-before`; diagnostic and controlled red/green
evidence is in `target/cleanup-forensics-replay.log` and
`target/cleanup-forensics-{controlled-red,controlled-green,ssh-green,signals-green}.log`.

## Recording rollback ownership race (Copilot follow-up)

At authentic head `1ba282f3`, the recorder's per-artifact rollback checked a
published link's device and inode, then unlinked its public name. A competing
writer could replace that entry between those operations. The permanent
`test_rollback_cannot_delete_a_competitor_arriving_after_ownership_check`
reproduces this with actual files: after a real first hard link and an injected
second-publication ENOSPC, the fixture atomically replaces the first entry at
the final unlink boundary. The old helper deletes the competing animation.

Recording publication now validates every staged entry, then moves the entire
private artifact directory to `OUTPUT/capture/` with Linux
`renameat2(RENAME_NOREPLACE)` through the held destination directory FD. There
are no partially published artifact links and no public-name rollback. An
existing capture, including one created after preflight, remains untouched.
Once the group becomes visible, cleanup is disarmed even if the requested
parent pathname subsequently changes; it preserves both the published group
and another writer's recreation of the old staging name.

All 17 recording tests pass. They exercise the real atomic directory move,
complete artifact bytes, existing and late competing captures, file/symlink
collisions, ENOSPC without partial publication, unsupported entries rejected
before publication, parent-path replacement, and foreign entries arriving
after publication. The ownership-race test also runs unchanged against the
authentic old helper: its legacy unlink-boundary schedule fails, while the
corrected helper preserves a competing recording at its single atomic
publication boundary. This helper-only change does not alter runtime source,
the release executable, or the already recorded animation.

The authentic helper is retained at
`target/git-operands-red/record_demo-1ba-before.py` with SHA-256
`7f9b277ab262e913e67d08fde23397e0927d108114e448821409f5198cf88072`.
The focused red result is in
`target/git-operands-red/copilot-recording-rollback-toctou-red.txt`; the complete
green suite is in
`target/git-operands-green/copilot-recording-grouped-publication-green.txt`.

## Distinct runtime and dev copies in the release lock closure

At authentic head `1ba282f3`, `binary_lock` selected root dependency edges only
by crate name. If runtime code used `shared` 1.x and tests used a renamed
`shared` 2.x, both copies and their transitive dependencies entered the binary
closure. Five permanent subcases failed before the correction: a dev-copy and
dev-transitive update, then renamed runtime, build, target-runtime and
target-build declarations. Every dev-only update incorrectly required a binary
release despite unchanged executable inputs.

The policy now selects root edges from runtime/build declarations and their
package/version requirements, then traverses the exact locked dependency
closure. It follows Cargo's
[version requirement rules](https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html#version-requirement-syntax)
for caret, tilde, wildcard, comparisons, intersections and prereleases. When
declarations cannot identify one locked root edge, classification fails with a
clear ambiguity error: the lockfile does not retain root dependency kinds, so
overlapping runtime/dev constraints cannot be guessed safely. Classification
continues to use only the Python standard library and performs no Cargo
resolution, downloads or network calls.

All 37 policy tests and 12 publication tests pass. Permanent checks retain the
five dev-only cases, a runtime/transitive update requiring a release, ambiguity
rejection and comparator boundaries. An isolated native Cargo oracle additionally
used only local path packages and `cargo metadata --offline`: its mixed
runtime/dev graph matched the selected closure, and all 42 version comparisons
matched Cargo. Native Cargo rejected the proposed partial-equality prerelease
change (`=1.2, >=1.2.3-alpha` for `1.2.3-alpha`); that discarded hypothesis is
not a claimed old defect or red result.

The authentic policy is retained at
`target/git-operands-red/release_policy-1ba-before.py` with SHA-256
`4e04317791a09a31a022edd0faed0d9662a088e083ef590d3d390e6b825434dd`.
Red/green logs are in
`target/git-operands-{red,green}/copilot-dev-runtime-collision.txt`; publication
results are in `target/git-operands-green/copilot-dev-runtime-publisher.txt`.
The native offline oracle is recorded in
`target/policy-cargo-offline-oracle/proof.json`.

## Preserve failed recording staging (Copilot follow-up)

At authentic head `9bccb75b`, the recorder recursively removed its staging
pathname after a failed recording. A competing writer could rename the original
held directory and create a different directory at that name. The permanent
`test_failed_recording_preserves_replacement_staging_directory_and_owned_log`
reproduces the failure with actual directories and files: its recorder fixture
writes owned diagnostics and a partial animation, moves that staging directory,
creates a replacement containing a foreign file, then fails. The authentic
helper deletes the replacement directory and the foreign file.

Failed staging is now deliberately retained for inspection. The helper performs
no recursive or name-based deletion of recording artifacts on failure. Before
closing the held staging FD it reports that directory's actual location and
points to `vhs.log` when present, including after the directory has been renamed.
A retry requires a fresh output directory; existing artifacts are rejected
before dependency probes. Successful recordings still appear together through
the single atomic move to `OUTPUT/capture/`.

All 19 recording tests pass. The new controlled replacement case preserves both
the original owned diagnostics/animation and the competing directory. A second
failure case retains the actual log from a renderer returning status 17 and
checks that retrying the same output cannot overwrite it. Publication-failure
tests now retain their staged files rather than expecting an empty output
directory. Runtime source, release version, executable and published media are
unaffected.

The authentic helper is retained at
`target/git-operands-red/record_demo-9bcc-before.py` with SHA-256
`6261db70db8a893016c942d1356077b214fc2b39d4aad9e37489afa56731e411`.
The focused red result is in
`target/git-operands-red/copilot-recording-failed-staging-red.txt`; the complete
green suite is in
`target/git-operands-green/copilot-recording-retained-failure-green.txt`.

## Anonymous WebP output avoids replacement-directory cleanup

The converter at authentic head `9bccb75b` had the same staging-name cleanup
defect. After a genuine Pillow encoding of an 8×6 two-frame GIF, a controlled
fixture moved the owned staging directory and recreated its original name with
a foreign recording. Validation then failed. `TemporaryDirectory` recursively
deleted that replacement and its foreign file, while the renamed owned
animation survived. The permanent
`test_failed_validation_preserves_a_replacement_staging_directory` failed against
the authentic helper because those competing bytes disappeared.

The converter now opens an anonymous `O_TMPFILE` inode through a held destination
directory FD. Pillow writes to its file handle; validation decodes a `BytesIO`
copy, leaving the publication descriptor owned by the helper. A successful
conversion publishes that inode with an exclusive `linkat` operation through
`/proc/self/fd`. Closing descriptors handles failures without named staging
directories or recursive deletion. The helper requires Linux, `/proc/self/fd`
and `O_TMPFILE` support on the destination filesystem, and reports an explicit
error when anonymous output is unsupported. It has no named-file fallback.

All ten conversion tests pass with real Pillow 11.3.0/libwebp 1.5.0, including
the existing eight output-preservation/codec cases, foreign-directory survival
after failed validation, and unsupported-filesystem failure without artifacts.
An actual CLI conversion also preserved all decoded RGB pixels, dimensions,
70/130 ms frame durations and loop count; publication returned the expected
size and SHA-256. Runtime source, version, executable and the large published
animation are unchanged.

The authentic converter is retained at
`target/git-operands-red/convert_demo-9bcc-before.py` with SHA-256
`919af473a91c5d2fdce80e98b117690c433a71598a3c4488f6c23be34a7cd14f`.
Red/green results are in
`target/git-operands-red/copilot-converter-staging-substitution.txt` and
`target/git-operands-green/copilot-converter-anonymous-output.txt`.
The original real-codec reproduction is recorded in
`target/converter-staging-substitution-red/proof.json`.

## Installer PATH directory aliases (Copilot follow-up)

At authentic head `302a3753`, the installer compared PATH directory spellings
before examining the selected executable. Two permanent shell-entrypoint tests
failed: an alias directory already selecting the installed binary received a
redundant prepend instruction, while an earlier executable before that alias
received no shadow warning. The fixtures use real directory symlinks and
verified offline release packages; a marker proves the earlier executable is
never run by these diagnostics.

The installer now checks the selected executable's `samefile` identity first.
Directory identity also determines whether a missing-location hint is needed.
Aliases selecting the installation emit neither warning nor prepend advice;
an earlier different binary is identified, and genuinely absent installation
directories still receive shell-quoted PATH guidance.

Both focused regressions and all 35 installer tests pass. The authentic script
is retained at `target/git-operands-red/install-302a-before.sh` with SHA-256
`263feedb7c189a3ac57cee027ee82d0a0fa50aaee85a230886d0e5f201291cfc`.
Red/green evidence is in
`target/git-operands-{red,green}/copilot-installer-path-directory-alias.txt`.
Runtime source, version, executable and animation are unchanged.

## Authenticate recording staging during publication (Copilot follow-up)

At authentic head `d78a95c7`, three permanent regressions fail: replacing the
staging directory before publication or at the rename syscall boundary produces
false success, while replacement after a successful recorder process redirects
the GIF read and creates metadata inside the foreign directory. The fixtures
perform real directory moves and the native atomic publication operation; the
successful-process fixture supplies observed tree/status/cleanup events without
running VHS or mounting FUSE. Its ordinary-success baseline already passes.

GIF reading and exclusive metadata creation now use the held staging FD, with
no-follow opening and regular-file validation for the GIF. Size and digest come
from the same read. Publication checks the source directory's device/inode
before rename and checks `capture` against the held staging FD afterward, before
reporting success. A substitution exactly at rename may move the foreign
directory; it is detected without rollback or deletion, and diagnostics report
the retained owned stage through its FD. All 23 recorder tests pass, preserving
both foreign bytes and owned logs/metadata in the controlled failures.

The authentic helper is retained at
`target/git-operands-red/record_demo-d78a-before.py` with SHA-256
`6701a0368fe09a801dd8ff9f8d08bbdd301ab903816856d27214d73760d1b00b`.
Red/green evidence is in
`target/git-operands-{red,green}/copilot-recording-stage-identity.txt`.
Runtime source, version, executable and animation are unchanged.

## POSIX installer review (2026-10-20)

The shell rewrite preserves the published three-asset release format. Independent
review found five defects in working candidates before publication. Each gained
a permanent black-box regression in `tests/test_installer.py` and was observed
failing before its fix:

- `test_duplicate_members_cannot_bypass_guard_with_early_exit`: a duplicate tar
  binary ending in `exit 0` installed successfully in candidate SHA-256
  `7c0207b3bbebb16289b839a1908e6d941783d0d76867080f0346aa436ba13652`.
  `set -e` is suppressed inside a conditional subshell; explicit failure exits now
  guard every archive-reading step.
- `test_directory_swap_before_cd_cannot_redirect_installation`: a controlled
  `mkdir` wrapper replaced the checked directory with a symlink. Without the
  post-`cd` physical-path check, installation returned success and replaced the
  foreign binary. The physical-path check now fails before staging.
- `test_relative_and_empty_path_entries_survive_directory_pinning`: candidate
  SHA-256 `cd232c99dbcb296a40a0afc85308d813760da1f1177ed85bb3b9c81cbb6fd39b`
  returned 127 (`stat: not found`) for `PATH=tools`. Entries are now made absolute
  relative to the invocation directory before `cd`.
- `test_signal_after_atomic_commit_reports_no_false_rollback`: a signal from a
  wrapper after successful `mv` produced a false “unchanged” diagnostic despite
  the new executable being installed. Interrupt diagnostics now avoid claiming
  rollback after the atomic commit; pre-commit preservation is tested separately.
- `test_version_probe_kills_a_process_ignoring_term`: a shortened test timeout
  could not stop a fixture ignoring SIGTERM and exceeded the six-second harness
  deadline. `--kill-after=2` now bounds both archive commands and the executable
  probe. The shortened deadline is test-only; production keeps a 15-second probe.

Focused regressions and the complete shell-installer suite pass after these
fixes. The Docker acceptance additionally installs the actual executable through
latest/pinned/upgrade paths with no Python/jq anywhere in its root filesystem,
network disabled, and verifies download/checksum failure preservation. These are
installation checks, not evidence of a real FUSE mount; exact-head CI separately
supplies real-mount acceptance.

### Shell installer follow-up review

Four additional review regressions reproduce failures at PR head
`04caed336791fb274e4dc3f73ec28e75f489e950`. Running the four named tests against
that installer/acceptance code produced three assertion failures and a six-second
probe deadline error before the fixes:

- `test_fresh_destinations_under_group_writable_umask`: both fresh default and
  custom destinations must work under `umask 0002`. Previously `mkdir` created
  mode 0775 and the permission guard rejected it. The private umask now applies
  before directory creation; an existing 0775 directory is still refused and
  retains its permissions and old executable.
- `test_probe_deadline_includes_child_processes`: a probe spawning `sleep 60`
  left inherited stdout open after a shortened one-second timeout. The probe now
  runs in timeout's process group; interrupted waits explicitly stop the owned,
  unreaped timeout leader and its group. The regression checks the child stopped,
  the old installation survived, and staging was removed.
- `test_destination_diagnostic_treats_shell_metacharacters_as_data`: paths with
  dollar signs, backticks and quotes were presented as an unsafe runnable shell
  command. The diagnostic now labels the literal executable path as data.
- `test_packaged_acceptance_rejects_duplicate_checksum_entries`: a manifest
  containing the archive once and `release.json` twice incorrectly passed the
  set-based acceptance guard. Comparing the sorted names with multiplicity now
  rejects it before installation, matching saved legacy installers.

Independent follow-up additionally reproduced two lifecycle failures in the first
correction: a child ignoring TERM survived when its parent exited on TERM, and a
successful version probe could leave a background child running. Permanent tests
`test_probe_timeout_stops_term_resistant_child_after_parent_exits` and
`test_successful_probe_stops_background_children` both failed before the final
probe-group cleanup. The monitored wrapper now writes the probe status, then
terminates its own private timeout group even on success. At the 15-second
deadline timeout sends KILL to that group; interruption cleanup only signals the
owned, unreaped timeout child/group. No numeric group is signalled after reaping.

The targeted regressions pass after their fixes. Only fixtures shorten the
production 15-second probe deadline. Release assets, binary inputs and the
package version are unchanged.

## 1.3 materialized host-collision diagnostics

Self-review found that the materializer safely refused `Foo`/`foo` on the
case-insensitive development volume but reported only `foo`, omitting the
already-created conflicting path. The strengthened permanent
`MaterializedTests.test_case_collision_fails_before_child` requires both names,
no child stdout, unchanged source and complete temporary cleanup.

Red: at `ad4e33efd927b4c35f03d55a67e816f8958c259d`, rebuild and run
`python3 tests/materialized.py target/debug/git-on MaterializedTests.test_case_collision_fails_before_child`.
It failed because `b'Foo'` was absent from stderr (exit 1); the child never ran.
Green: the corrected diagnostic includes the attempted snapshot path and the
existing host directory entries. The same test passes on arm64 macOS 26.3.1(a).
Local transcripts are `/tmp/git-on-collision-red.log` and
`/tmp/git-on-collision-green.log`. This is materialization evidence, not macFUSE.

## 1.3 Copilot: optional passthrough must preserve older INIT negotiation

At `af8791f`, enabling ABI 7.40 inherited upstream 0.16's unconditional 64-byte
INIT decode. The pre-existing 16-byte INIT unit fixture failed with
`InsufficientData` using:
`cargo test --manifest-path vendor/fuser/Cargo.toml --lib --features abi-7-40 ll::request::tests::init -- --exact`.
The red transcript is `/tmp/git-on-init-red.log`.

The reviewed patch decodes the four-word prefix, requires the extension only
when FUSE_INIT_EXT is advertised, preserves high capability bits, clamps the
reply minor and reflects the client's extension flag. The original short test
and new extended/truncated-extension parser test pass; all five parser tests
pass in `/tmp/git-on-init-green.log`. The permanent actual-dispatch regression
`protocol_short_and_extended_init_preserve_ordinary_fuse` additionally covers
both session handshakes on Linux. Parser/protocol tests are not real mounts.

## 1.3 Copilot: disposable write-policy harness

At `af8791f`, `WritePolicyTests.policy` retained an undefined `keep` expansion.
`python3 tests/test_write_policy_harness.py` reproduced NameError before any
command launch (red: `/tmp/git-on-keep-harness-red.log`). Removing that expansion
preserves exact argv and passes the permanent launcher regression (green:
`/tmp/git-on-keep-harness-green.log`). Real policy tests remain separately required.

Copilot also identified the Ubuntu 22.04/Python3.10 tomllib assumption and
cross-job rehash weakness. Assembly now reads Cargo's JSON metadata and the
four permanent `test_dist_integrity.py` cases reject changed native archives,
missing/malformed checksums and source/manifest mismatches before regeneration.

The packaging invariants also have permanent workflow-contract regressions.
`GIT_ON_CI_BEFORE_REF=fc01cbc python3 tests/test_ci_contract.py` fails the missing
pre-generation integrity gate and hard-coded 1.3.0 binary probe. The same suite
passes against the corrected workflow (`/tmp/git-on-ci-contract-{red,green}.log`).
The current probe obtains the package version from Cargo's JSON metadata and
therefore retains the Ubuntu 22.04/Python3.10 build baseline.

## 1.3 native cargo-dist checksum whitespace

Exact-head CI exposed that cargo-dist 0.33.0 writes a trailing blank line in
native per-archive checksum files. At `0f49816`, the new incoming-integrity guard
accepted only one terminal newline and rejected that authentic output.
`NativeIntegrityTests.test_cargo_dist_checksum_trailing_blank_line` reproduces
that failure (`/tmp/git-on-dist-blank-red.log`). The guard now trims surrounding
whitespace before strict whole-record validation; embedded extra records,
malformed hashes and wrong artifact names still fail. All five integrity
regressions pass (`/tmp/git-on-dist-blank-green.log`).

The complete generated Apple Silicon installer is also exercised with GNU
`sha256sum` excluded from PATH, leaving stock `/usr/bin/shasum`. Actual install,
repeat update and corrupted-archive preservation pass locally in
`/tmp/git-on-generated-shasum.log`; the same scenario is now a macOS packaging
CI step (`tests/dist_installer.py --without-gnu-hash`).

## 1.3 Copilot follow-up: destination staging and vendored release policy

At `23f5fca`, `tests/dist_installer.py` interrupted the actual generated installer
after its first real move into destination staging. SIGINT returned 130 but left
both destination `tmp.*` directories, including the staged executable
(`/tmp/git-on-staging-sigint-red.log`). An injected staging failure independently
reproduced the leak (`/tmp/git-on-staging-red.log`). The hardening correction now
initializes both staging globals before registering cleanup, and removes them
along with the download directory. Inherited staging values cannot authorize
deleting unrelated paths. The permanent generated-installer regression passes
for staging failure, SIGINT, SIGTERM and SIGHUP, preserving installed bytes and
an unrelated sentinel; it also passes with stock macOS shasum alone
(`/tmp/git-on-staging-green.log`, `/tmp/git-on-staging-shasum-green.log`). The same
actual-installer test runs on both packaging CI platforms.

The permanent vendored-docs/tests/examples policy regression failed at
`23f5fca`, demanding a binary version bump for nonruntime edits
(`/tmp/git-on-vendor-policy-red.log`). Restricting the vendored classification to
runtime sources, manifests and build inputs preserves AGENTS.md's nonrelease
exceptions. All 41 release-policy tests pass after the correction, including
runtime source and isolated manifest/build-input cases that still require a
release (`/tmp/git-on-vendor-policy-green.log`).

## 1.3 Copilot follow-up: locked materialized directories

At `897088d`, the permanent normal-execution and explicit-recovery regressions
in `tests/materialized.py` both failed with exit 125 / Permission denied when
the disposable view contained populated mode-0555 and mode-000 directories.
The view root itself was also mode 000. The red transcript is
`/tmp/git-on-materialized-perms-red.log`.

After mount-ownership/unmount checks, both cleanup paths now restore owner
directory access through pinned directory descriptors, O_NOFOLLOW and no-follow
stat/chmod operations. Only directories owned by the current identity qualify.
An external read-only directory reached by a symlink keeps its original mode
and bytes. All seven actual Mac materialized cases pass after rebuilding,
including both permanent cleanup regressions
(`/tmp/git-on-materialized-perms-green.log`). This is non-FUSE evidence;
Linux CI separately runs the same cases and real-FUSE lifecycle tests.

## 1.3 macOS source audit: exclusive rename flags

Auditing the FSKit transport found that upstream macFUSE FUSE_RENAME carries
exclusive/swap flags in high bits, but the vendored dispatcher unconditionally
passed zero to the filesystem callback. The permanent actual-dispatch test
`macos_exclusive_rename_flags_survive_actual_dispatch` failed with observed 0
versus expected portable no-replace flag 1
(`/tmp/git-on-macos-rename-red.log`). The parser now preserves and translates
these flags; unsupported exchange still fails in Overlay rather than silently
becoming replacement. The same dispatch test passes afterward
(`/tmp/git-on-macos-rename-green.log`). This private protocol peer is not a mount.

## 1.3 enabled-passthrough harness secure execution

The first authorized capable-daemon CI at `a5dec76` proved actual bypass (eight
registrations and zero generated I/O callbacks) but the lifecycle harness failed
before its experiment: file-capability secure execution strips TMPDIR, so the
daemon used the system temporary parent. The permanent harness now accepts that
parent only in explicit enabled mode and validates the exact private owned run
and recovery format reported by its own child. At `f3e0b24`, all enabled and
ordinary-FUSE lifecycle cases passed with checked cleanup. CI printed the
capability-revocation and private-copy-deletion receipt; the linked evidence is
in docs/platforms.md. No mount skip is counted as this result.
