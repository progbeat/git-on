# git-on behavioral and performance specification

This document defines required behavior. The implementation status below is
separate from that contract; a requirement is not a claim of a passing test.

## Command and revision

- `git on [REPOSITORY] REV [OPTIONS] -- COMMAND [ARGS...]` selects a local
  revision, or a source and revision when REPOSITORY is supplied.
  Resolve the selected Git tree once and launch the given argv from
  its root without introducing a shell.
- The selected tree, not the index or working directory, determines source
  contents throughout the run. Later changes to refs do not change that tree.
- The command remains an ordinary program: standard streams, filesystem errors,
  and inherited environment retain their documented meanings. stdin, stdout,
  and stderr are inherited directly; git-on does not capture or tee output.
- Only `GIT_ON_TREE_OID` is added or overridden in the child environment. No run
  ID, output-directory, or run-directory variable is synthesized or consumed.
  Caller-provided variables with such names remain ordinarily inherited.
- Every child-command invocation requires a `--` separator. The first `--`
  always ends runner argument parsing; it cannot escape a revision. Before it,
  accept one operand for the local revision or two for repository and
  revision. Recognized `--verify` and
  `--write-policy=none|ignored|untracked|all` options may appear anywhere before
  the separator, interspersed with those operands. Boolean options may
  repeat and combine idempotently. Parse only git-on's recognized options;
  every other token is a Git operand, including an empty or option-looking
  token. Preserve its original bytes without splitting or normalization.
- Everything after the first separator is literal command argv, including a
  command name beginning with `-`, unknown options, `--keep`, `-k`, and further
  `--` tokens. Empty later arguments and non-UTF-8 bytes are preserved. No shell
  or string re-tokenization is introduced.
- A missing separator, missing revision operand, more than two positional operands,
  malformed git-on option, or missing/empty command name returns usage status 2
  with a clear diagnostic before Git resolution or runtime-state creation.
  First-token `--help`/`-h`, `--version` and `--cleanup` retain their separate
  existing handling and do not require a child-command separator. These
  spellings after the separator are ordinary command argv.
- Migration from 0.1.x requires inserting the separator before every command:
  `git on REV command args` becomes `git on REV -- command args`.
  `--keep` and `-k` before the separator are removed options and return usage 2.
  Save required logs and artifacts to explicit external paths.
- git-on has no built-in execution logs, durable run manifests, artifact export,
  or experiment-recording layer. Callers may record output externally.

## Native Git argument compatibility

This compatibility is part of the original command contract. Corrections that
restore native Git argument semantics are bug fixes, not a new git-on grammar.
Native Git is the resolution authority for both local and remote operands.
Consequently, direct local `git-on` execution also requires Git on `PATH`, even
where the former Git-free resolver accepted a revision. Retiring that resolver
is part of this bug fix; it does not require a major version increment.

- With one operand, pass that exact `OsStr` as the revision argument to native
  `git rev-parse --verify --end-of-options`. Git interprets its revision syntax,
  including abbreviations, ancestry, reflogs and `HEAD:path`. Native Git is
  required for local resolution as well as remote operations.
- With two operands, pass the exact repository and revision/refspec arguments
  to native `git fetch` in a private temporary Git repository. Git resolves
  configured remotes, URLs, local paths, ref names, refspecs and their precedence.
  A colon in this second operand is fetch syntax; git-on must not reinterpret
  it as a remote subtree selector or strip a `+` prefix.
- Do not implement an additional repository/revision grammar, reject operands
  by character set or encoding, trim them, expand names, duplicate Git's
  ambiguity checks or introduce host-specific resolution. Delegate acceptance
  and diagnostics to the native Git operation, with argument boundaries that
  prevent an operand from becoming an internal Git option. git-on's own option
  and command-boundary checks remain its responsibility.
- Resolve once before executing the command. The native fetch result must
  identify one distinct selected object; a result with no object or multiple
  different objects cannot identify the single snapshot. The selected object
  must yield a Git tree. These are checks of Git's result, not operand syntax.
  Signature verification examines the exact selected commit or annotated tag;
  a tree returned by `HEAD:path` has no signature to verify.
- Caller refs, index and working files must stay untouched. Fetch destinations
  are confined to the private repository. Existing object-demand, filter,
  cancellation, SSH ownership, cache and filesystem contracts still apply;
  materialization cannot bypass any Git resolution, verification or object error.
- Native operations use the caller's Git configuration and object-location
  environment. Resolve local revisions and read the objects from the same
  Git-selected object database, including object directories and alternates.
  Relative object locations follow native Git's repository setup context,
  including worktree subdirectories, bare repositories and explicit `GIT_DIR`.
  Native fetch can invoke configured Git hooks for its private ref updates;
  git-on does not introduce a checkout or run checkout filters on snapshot
  content. Hooks and other configured helpers are caller programs with host
  access, like the explicitly executed command; their side effects are not
  confined by the snapshot's write policy.
- Regression tests must compare acceptance, selected objects and diagnostics
  against native Git in isolated fixture repositories. Include native ref
  spellings and prefixes, colliding branch/tag names, literal fetch refspecs,
  local paths, non-UTF-8 operands and invalid inputs. A short hash accepted by
  local revision resolution must not be assumed accepted by remote fetch.

## Filesystem policy

Remote demand, persistent cache lifetime, native verification, authentication
limits and acceptance requirements are normative in [the remote guide](remote.md).
The same FUSE and write-policy contract below applies to local and remote runs.

- `--write-policy=ignored` is the default and preserves the existing contract.
  Entries present in the selected tree are immutable. An ignore match does not
  permit changing, replacing, removing, renaming, or shadowing tracked entries.
- In `ignored`, new paths are writable only when ignored by applicable `.gitignore` files in
  the selected tree, using Git's nested, negated, anchored, and ignored-parent
  semantics. Host-global rules, `.git/info/exclude`, working-tree edits, and new
  runtime `.gitignore` files do not alter this policy.
- `--write-policy=none` makes the entire mount read-only, including ignored paths,
  contents, namespace operations, links, and metadata changes.
- `--write-policy=untracked` allows creating and mutating paths absent from the
  selected Git tree without consulting ignore rules. The index and working copy
  do not define tracking. Selected-tree entries remain immutable, including
  their parent directories: rename, deletion and replacement cannot bypass
  tracked-file protection. New paths inside a tracked directory are permitted.
- `--write-policy=all` permits ordinary file creation, modification, deletion,
  rename, links and supported metadata changes within the mount, independent of
  ignore rules. Source payloads are copied only when required for mutation;
  directory renames keep demand-loaded source origins. Git objects, the index,
  and the working copy stay unchanged. Unsupported filesystem operations retain
  their existing error contracts.
- The same rule governs all programs and languages. No Python, uv, Numba, cache,
  or environment-directory exception exists.
- A disallowed filesystem operation returns an appropriate ordinary filesystem
  error to the caller. Denial alone does not make git-on kill the application.
  The application may handle the error, continue, or exit normally.
- The writable layer is fresh for each run. It uses private filesystem-backed
  storage under the system temporary location and respects `TMPDIR`; git-on
  creates no private memory filesystem and imposes no disk quota. The underlying
  temporary filesystem's capacity and operating-system limits still apply.
- Completion unmounts (when mounted) and deletes the private execution tree.
  `--keep` and `-k` are removed before the separator; child argv remains literal.
  Host paths explicitly written by the command are outside this cleanup scope.
- Abrupt process or machine termination and busy mounts may require stale-run
  cleanup. Minimal temporary recovery metadata is permitted for mount identity,
  locking, releasing the exact Git pin and identifying the owned SSH socket;
  it is not a run manifest or log.
  `git on --cleanup <temporary-directory>` must refuse active directories, avoid unrelated mounts and changed pin targets, and
  never terminate processes based on stale PIDs.
- The filesystem is not a host security sandbox. This contract does not promise
  isolation from external files, processes, networks, or inherited credentials.

## Backend selection and materialization

FUSE is preferred whenever available and capable of the requested behavior.
Missing Linux kernel passthrough is only an optimization limitation and retains
ordinary FUSE I/O, including on macOS. Before launching the child, discovery
identifies absent/inaccessible `/dev/fuse` or `fusermount3` on Linux, or an absent
GitOn.app native Apple FSKit extension bundle on macOS. Native mounting
requires owner-approved bundle installation, signing and extension activation.
Linux uses the published fuser library; macOS has no macFUSE dependency.
Only explicit `--write-policy=all` permits an
ordinary private-directory backend in that case. Other mount failures remain
errors. Never rerun a command after a mount or runtime failure.

FSKit is the preferred macOS path without a kernel extension. Its missing
per-file fsync callback is an explicit platform limitation, not an equivalent
durability/error-propagation contract. Persistent databases belong outside the
disposable execution tree; crash-durable temporary databases are not required.
Required write policies, permissions, isolation and lifecycle behavior remain
unchanged. See [the platform constraints](platforms.md) for separate startup,
FSKit and read-only-mount version requirements and actual verification status.

The materialized backend reads the same pinned tree eagerly, preserves bytes,
symlinks and executable modes, and never applies checkout filters or fabricates
`.git`. Unsupported objects/names, case or normalization collisions and host
symlink/permission limitations fail before launch, rather than overwriting,
renaming or omitting entries. This can fail on an entry the child would not use.
It costs full-tree startup time, network transfers and temporary disk space.
Host filesystem operations, xattrs and locks follow the host's ordinary behavior;
this backend implements the explicit-all subset, not protected FUSE policies.
The lazy startup/performance requirements below apply to the FUSE backend.

## FUSE operation coverage requirement

Every callback in the locked `fuser::Filesystem` trait must have an explicit,
reviewed entry in the [FUSE acceptance matrix](fuse-coverage.md). The inventory
must distinguish application overrides, inherited lifecycle defaults, inherited
unsupported handlers, and operations excluded by the supported Linux platform.
A dependency update or newly added callback must fail the completeness guard
until its contract and evidence are reviewed.

For every Linux callback, tests must exercise actual fuser dispatch into the
real filesystem or inherited handler and assert its reply/state contract.
Supported operations require positive behavior and meaningful negative or
boundary cases where applicable. Rejections must check that protected bytes and
namespace state remain unchanged. Mount-bypassed operations, credentials and
invalid handles may be tested using the protocol harness; such tests must never
be reported as real mounts.

Real-FUSE acceptance must additionally verify supported syscall behavior and
intentional kernel fallbacks, including creation, synchronization, statistics,
rename flags, directory pagination and unsupported optional operations.
`DefaultPermissions`, unnegotiated capabilities and kernel caching must be
documented when they bypass callbacks. In particular, syscall success does not
prove that an unsupported callback is implemented, and ordinary file creation
does not substitute for testing `mknod`.

The matrix must map operations to existing named tests, distinguish default
`ENOSYS` replies from Linux's observable fallback/errors, and record platform
exclusions and limits. CI must check inventory/reference completeness and run
both protocol tests and non-skipped real-FUSE tests on the exact revision under
review. A passing inventory guard is not execution evidence; passing scenarios
are not an assertion of exhaustive inputs, all host failures, or 100% branch
coverage. Material discovered coverage gaps become regression cases before the
affected behavior is claimed verified.

## Lazy access and performance

The objective is minimal launch overhead and demand-driven work, including for
repositories with millions of paths. FUSE must not be a wrapper around a full
checkout or a complete in-memory copy of the selected tree.

1. Before launching the child, git-on must not recursively enumerate the entire
   selected tree, load every blob, or materialize all source files. Resolving and
   pinning the selected tree and establishing the mount are permitted startup
   work. Unvisited subtrees must not be traversed merely to start the command.
2. Lookup, attributes, directory listing, and reads load metadata or contents
   needed for those operations. Listing a directory may decode that directory's
   tree object, but must not recursively enumerate its children. Resolving a
   deep path may read its ancestor trees.
3. File contents are loaded on demand. A lookup or directory listing must not
   read unrelated file payloads. Blob headers, applicable ignore files, and
   required Git object decoding are permitted when necessary for the operation.
4. Git storage encoding may require decoding a whole object or its delta bases,
   even for a partial read. This is not permission to preload unrelated trees
   or payloads. The implementation must distinguish requested content from
   necessary storage-level work when reporting measurements.
5. Write-policy checks must load applicable pinned ignore rules on demand and
   check relevant tracked paths without a whole-tree scan. Protecting a tracked
   ancestor does not require enumerating all its descendants.
6. Repeated operations should reuse useful decoded data instead of repeatedly
   decoding the same objects. Caching must have an explicit bounded retention
   policy; walking many files must not retain every payload indefinitely.
   Active reads and unavoidable object decoding are accounted for separately.
7. Do not keep duplicate full source payloads merely to transfer data between
   snapshot and filesystem layers. Memory use should reflect demanded objects,
   active operations, and bounded caches, not the whole repository's contents.
8. Failures discovered in objects reached on demand must be reported through
   the corresponding operation or documented runner failure. Whole-repository
   validation must not be introduced as an implicit startup prerequisite.

No numerical latency, throughput, or memory SLA has been agreed. “Fast” does
not establish a particular number of milliseconds. Measurements must report
hardware, software, storage, cache state, fixture shape, and the tested revision.

## Acceptance evidence

The following checks establish the lazy contract independently of ordinary
functional correctness:

- Build exactly one million leaf files for six-digit strings `000000` through
  `999999`. The first five digits are nested directories and the sixth is the
  filename: `123456` maps to `1/2/3/4/5/6`. Each directory has ten entries.
- Each leaf contains its six-digit number (with a documented optional newline).
  The benchmark harness chooses a seeded random number before timing, then runs
  `git on <tree-OID> -- cat <digit-path>`. Record the seed and chosen number and
  verify externally that stdout matches that number. No Python child script or
  its interpreter startup is required in the measured workload.
- Measure from git-on invocation until that correct stdout is observed, as well
  as child-start latency, total time, and peak memory where available. Fixture
  construction is excluded. Repeat trials and identify cold/warm cache state.
- Smaller fixtures can diagnose scaling, but do not replace the specified
  million-file case. Keep file contents and object reuse explicit in results.
- Observe Git object accesses where instrumentation is available. A no-op
  command must not cause a recursive tree walk; a one-file read must not load
  unrelated subtree contents. The opt-in `test-metrics` build may write counters
  to an explicit inherited `GIT_ON_TEST_METRICS_FD` pipe supplied by the test
  harness. Normal builds have no metrics channel, and instrumentation must not
  capture or replace the command's standard streams. Without such
  instrumentation, timings alone are not proof of lazy object access.
- Exercise lookup, `stat`, directory listing, one-file reads, repeated reads,
  and large-file partial reads separately. Record decoding dependencies and
  bytes where observable rather than inferring them from wall time.
- Visit more content than the cache retention budget and verify eviction; do
  not count unavoidable kernel page cache as a second application-owned payload.
- Run the existing immutable-source, ignore-policy, lifecycle, and real-FUSE
  integration checks after changing the loading architecture. Performance must
  not weaken these semantics.

A benchmark fixture that reuses Git objects must disclose this: it measures
logical namespace growth but does not by itself model a million distinct blobs.

## Review regressions and bounded randomized stress

Every behavior defect found during review must gain a focused regression test
that reproduces the defect before its fix and passes afterward. Preserve the
trigger and assert the corrected observable result, including failed-operation
postconditions. Record the old revision, reproducer and corrected evidence in
[review regressions](review-regressions.md). Test and documentation defects need
an appropriate guard or targeted check too. For nondeterministic races, retain
the observed failure and explain the controlled regression strategy; never
invent a deterministic red result or claim exhaustive interleaving coverage.

A seeded randomized real-FUSE test must exercise bounded sequences of file and
directory operations against a model or differential oracle. Compare operation
results, applicable errors, contents, namespace and relevant metadata, including
immutable-source and pinned-ignore protection, after each step. On failure,
report the seed and executed operation trace for replay. Explicit limits on
operation count, generated entries, file sizes and elapsed test time keep CI
safe; these are test-harness limits, not quotas imposed on user commands.
Random stress supplements targeted regressions and the callback matrix; it does
not replace them or establish proof for all possible sequences.

## Implementation and verification

The second implementation replaces the eager traversal with a shared pinned
object store, demand-only metadata/blob access, bounded caches, and deferred
inode creation. The FUSE layer retains no duplicate snapshot payloads. Source
errors are reported when the relevant operation reaches them: submodules fail
metadata access, LFS pointers fail content reads, and unrelated entries remain
usable. Listing a directory may report a submodule name without following it.

Local unit tests exercise demand counters, unvisited missing objects, cache
retention/eviction, policy isolation, inode lifetimes, and active large-file
spooling. Real-FUSE regression and million-file performance results must be
verified on the exact published revision in GitHub Actions. Consult the PR's
linked run for that verification; local tests alone are not a real-mount pass.
