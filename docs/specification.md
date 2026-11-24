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
  revision. Recognized `--verify`, `--backend=auto|fuse|nfs|copy` and
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

## Service state

Git-on may read caller files and repositories and obtain data from remote
sources. During preparation, execution, teardown, failure, cancellation, and
stale-run cleanup, Git-on and its service operations keep their writes inside
run-owned storage. The runner may write ordinary output to stdout and stderr.

Signature verification with `--verify` is an exception to the service write
boundary. Git-on calls `git verify-commit` or `git verify-tag` on the selected
object ID. Git and its configured verifier use the caller's effective
configuration and the verifier's ordinary environment. Direct verification
may write to its normal profile or to other paths selected by its configuration.
Git and its verifier own those effects, including on failure or cancellation.
Git-on cleans up the files it creates for the run. The write policy governs
the command after `--`.

- Resolution, fetch, snapshot preparation, and cleanup preserve source refs,
  indexes, object databases, Git configuration, and worktrees. These operations
  leave no persistent clone and make no requests to write remote repository data.
- Native fetch keeps its selection record and private refs in the run
  directory. Incoming Git objects, pack indexes, and temporary files required
  by the transport prefer an existing writable `tmpfs`. Snapshot selection
  and verification reuse the received objects in place.
  A pack write that fills an automatically selected `tmpfs` can retry on
  the run filesystem before native Git records its selection.
- Native resolution and fetch disable hooks and local filesystem monitors. Credential
  lookup reads existing credentials. An authentication result does not save,
  erase, or replace stored credentials, host keys, or cookies.
- Cleanup can remove owned runtime data. Recovery metadata from an older
  invocation does not authorize Git-on to write to a source repository.
- The writable snapshot and actions of the command after `--` follow the
  [filesystem policy](#filesystem-policy). This policy does not grant extra
  write access to preparation or cleanup processes.

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
- With two operands, native Git resolves configured remotes, URLs and local
  paths. The [reusable transport](remote.md#object-demand-and-cache-lifetime)
  can select an unambiguous branch or full object ID. A source-only force
  refspec can use the same selection when default fetch mappings make a private
  destination update unnecessary. The compatibility path
  passes the exact repository and revision/refspec arguments to native
  `git fetch` in a private temporary Git repository for ref selection and
  precedence.
  That path keeps any `+` prefix. A colon in this operand is fetch syntax
  and never selects a remote subtree.
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
  Signature verification uses the full OID of the selected commit or annotated
  tag. For a tag, derive the snapshot from the target recorded in the verified
  tag object. Later movements of the tag or branch name leave the selected
  object unchanged. A failed check or an unavailable selected object before
  tree preparation completes prevents the command from starting.
- Fetch destinations are confined to the private repository. Existing object-demand, filter,
  cancellation, SSH ownership, cache and filesystem contracts still apply;
  materialization cannot bypass any Git resolution, verification or object error.
- Git-on accesses the caller's local object database read-only and retains the
  selected tree OID in memory for the invocation. If external repository
  maintenance removes an unread object, the operation that needs it reports the
  source error.
- Native operations use the caller's Git configuration and object-location
  environment. Resolve local revisions and read the objects from the same
  Git-selected object database, including object directories and alternates.
  Relative object locations follow native Git's repository setup context,
  including worktree subdirectories, bare repositories and explicit `GIT_DIR`.
  Native operations apply the [service-state invariant](#service-state) while
  resolving and fetching objects. Git-on reads the snapshot directly from Git
  objects and uses its own materializer for a disposable execution tree.
- Regression tests must compare acceptance, selected objects and diagnostics
  against native Git in isolated fixture repositories. Include native ref
  spellings and prefixes, colliding branch/tag names, literal fetch refspecs,
  local paths, non-UTF-8 operands and invalid inputs. A short hash accepted by
  local revision resolution must not be assumed accepted by remote fetch.

## Filesystem policy

Remote demand, run-local object lifetime, native verification, authentication
limits and acceptance requirements are normative in [the remote guide](remote.md).
The native mount and write-policy contracts apply to local and remote runs.

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
  storage in a per-run directory under `TMPDIR/git-on` (or the system temporary
  location's `git-on` directory). If another unprivileged user owns that group
  or an ordinary file occupies its name, git-on uses a private group for the current
  user. The writable layer uses the underlying temporary filesystem's capacity
  and operating-system limits. Native service files can use a separate location.
- Completion unmounts the snapshot and removes the run's temporary files.
  The last run removes its empty group.
  When an NFS client defers removal because a file is still open, git-on
  reports the run path and preserves the command's exit status. The diagnostic
  gives a later `--cleanup` command when automatic recovery can verify the
  record, and identifies manual recovery when the surviving directory lacks
  a persistent identity.
  `--keep` and `-k` are removed before the separator; child argv remains literal.
  Host paths explicitly written by the command are outside this cleanup scope.
- Handle recoverable resource errors through ordinary operation or runner
  failures and orderly cleanup whenever possible. Manual stale-run recovery
  does not justify abrupt termination for an error that can be handled.
- Abrupt termination and busy mounts may require stale-run cleanup.
  An empty, writable ownership marker locks the run from reservation to
  teardown, including on NFS. Startup rollback and ordinary cleanup can
  retire the marker without allocating another inode.
  `git on --cleanup <temporary-directory>` accepts legacy records and unused
  reservations. It refuses active runs and preserves directories, mounts, or
  endpoints whose identity it cannot verify. It can reclaim verified service
  directories after their helper exits. If removal must be retried, it keeps
  the run record. Older Git pins remain in the source repository.
  Cleanup never terminates processes based on stale PIDs.
- The filesystem is not a host security sandbox. This contract does not promise
  isolation from external files, processes, networks, or inherited credentials.

### Resource assumptions

Supported workloads fit in available RAM, including the aggregate data
needed to decode and retain a pack. Git-on leaves inherited process
limits, including `RLIMIT_FSIZE`, to the host. It follows its own cache
policy independently of those limits. The two-checkout budget concerns
available filesystem space for Git-on and the command.
Regular files required by native Git use the capacity and limits of the
selected temporary filesystem. `GIT_ON_SERVICE_TMPDIR` can select an existing
writable `tmpfs`. The verifier uses its own environment and storage limits.

## Backend selection and materialization

[Startup indication](startup-indication.md) defines progress display, terminal
ownership, and clearing before command execution.

`--backend=auto` is the default. On Linux it selects FUSE when available.
Missing kernel passthrough is only an optimization limitation and retains
ordinary FUSE I/O. Before launching the child, discovery identifies absent or
inaccessible `/dev/fuse` or `fusermount3`. macOS 26 and later use the built-in
client with the [private NFSv4.1 server](nfs.md). The macOS CLI runs on macOS 13
or later.

An explicit `--backend=fuse` or `--backend=nfs` requires that mount.
`--backend=copy` requires explicit `--write-policy=all` and creates an eager
temporary copy of the pinned tree. Auto permits the same copy with explicit
`--write-policy=all` when a native backend is unavailable. A macOS mount
attempt may fall back after checked cleanup of the partial mount and before
launching the command. Git resolution, verification, object loading and child
execution errors must propagate. Never rerun a command after a mount or
runtime failure. See the
[platform requirements](platforms.md).

The copy backend reads the same pinned tree eagerly, preserves bytes,
symlinks and executable modes, and never applies checkout filters or fabricates
`.git`. Unsupported objects/names, case or normalization collisions and host
symlink/permission limitations fail before launch, rather than overwriting,
renaming or omitting entries. This can fail on an entry the child would not use.
It costs full-tree startup time, network transfers and temporary disk space.
Host filesystem operations, xattrs and locks follow the host's ordinary behavior;
this backend implements the explicit-all subset.
The lazy startup and performance requirements below apply to both native mounts.

Remote materialization obtains the selected revision in one shallow transfer.
A mount fallback reuses the original selection and fetches the full snapshot
in one additional shallow transfer.
Verified contents are written to their final paths, and already prepared
contents are reused for repeated OIDs.

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
repositories with millions of paths. Native mounts must avoid full checkouts
and eager in-memory materialization of the revision's file contents.

1. Before launching the child, git-on must not recursively enumerate the entire
   selected tree, load every blob, or materialize all source files. Resolving and
   pinning the selected tree and establishing the mount are permitted startup
   work. Unvisited subtrees must not be traversed merely to start the command.
2. Lookup, attributes, directory listing, and reads load metadata or contents
   needed for those operations. Listing a directory may decode that directory's
   tree object, but must not recursively enumerate its children. Resolving a
   deep path may read its ancestor trees.
3. File contents are loaded on demand. Directory listings alone do not load
   file payloads. On reusable remote transports, the first actual request for
   a regular-file OID in an opened directory can schedule subsequent OIDs in
   depth-first order through already opened directories. The window starts at
   four objects. When first accesses confirm earlier forecasts, the planner
   can group remaining files from the current and recently opened directories.
   A lookup in an
   unopened directory may load only the necessary blob headers, applicable
   ignore files, and required Git object decoding.
4. Assume individual Git objects fit in RAM. A lazy read may decode and verify
   a whole object even for a partial read. Git storage encoding may also require
   decoding delta bases. Remote read-ahead uses the already parsed directory
   view and does not open additional trees. Measurements distinguish requested
   objects, speculative transfers, and necessary storage-level decoding.
5. Write-policy checks must load applicable pinned ignore rules on demand and
   check relevant tracked paths without a whole-tree scan. Protecting a tracked
   ancestor does not require enumerating all its descendants.
6. Repeated operations should reuse useful decoded data instead of repeatedly
   decoding the same objects. Bound the block LRU by bytes and the hot whole-blob
   LRU by object count. Account for active reads, decoding, and metadata
   separately from those caches.
7. Keep prepared Git objects immutable and verify the bytes served from them.
   The first successful content change switches the file to private writable
   backing. Copy-up uses an ordinary filesystem copy or clone. The entire
   private file uses ordinary filesystem I/O, including bytes retained from
   the source. Block authentication is confined to immutable source reads.
   If the same command can finish with two checked-out copies of the selected
   revision on disk, Git-on must be able to run it too.
8. Failures discovered in objects reached on demand must be reported through
   the corresponding operation or documented runner failure. Whole-repository
   validation must not be introduced as an implicit startup prerequisite.

The explicit [interview acceptance contract](interview.md#interview) is mandatory:

T_git_on <= T_materialize_same_revision + T_same_command_direct + O(N).

Compare one complete invocation with git clone/git checkout materializing the
same complete selected revision and then the identical direct-disk command.
Declare the same host/filesystem, source/transport, initial object and OS-cache
conditions; separate cold and warm cases. Account for cleanup explicitly on both
sides, without excluding Git-on's unmount or cleanup from T_git_on. Setup, every
process, fetch, decode, verification, metadata operation, traversal, internal
read/copy/write and cleanup are included in the total cost.

N is command-facing transferred read/write payload bytes plus metadata API
result bytes, including stat. Use the documented native API/ABI result layout
for that platform (for example, the public stat structure for a native stat API),
or a declared fixed equivalent for a higher-level API. Fix the representation
before measurement and use it consistently on both sides. Do not count language
heap allocation, internal wire/storage/index bytes or repeated passes as N.
Do not add artificial padding or count the same interface bytes twice.
Failed metadata calls contribute their public result/status representation, not
an unreturned stat payload. Zero-byte operations do not justify excluding their
execution cost or inventing internal bytes; N=0 cases still use the complete
materialize-plus-command comparison.

One-time required fetch, decode and verification may happen lazily within the
materialization budget. Immutable data already obtained/prepared must not be
fetched or unpacked again in the same invocation, including close/reopen and
decoded-memory-cache eviction. Invocation-local temporary storage must support
that reuse; no global cache or cross-run reuse is introduced. Count all work,
including native Git children, rather than proving this contract from loader-only
counters. Amortized/expected Big-O does not justify a large constant: minimizing
measured real overhead also remains required.

No numerical latency, throughput, or memory SLA has been agreed. “Fast” does
not establish a particular number of milliseconds. Measurements must report
hardware, software, storage, cache state, fixture shape, and the tested revision.

## Storage and buffer requirements

1. Internally managed object buffers use disk for LRU eviction and for the
   private regular files required by native Git compatibility. Git-on may
   read existing repositories from disk. The visible execution tree from
   `copy` and user writes, including copy-up and native-mount backing files,
   may use disk.
2. Immutable data already in RAM is used without copying on every execution
   path. Components borrow existing buffers or transfer ownership of them
   without copying their contents.
3. Keep the revision's loaded directory metadata and other internal metadata
   in RAM. Native mounts follow the lazy access requirements when loading
   metadata.
4. Remove redundant passes over data when the performance gain justifies the
   complexity. Where practical, compute and cache needed results as the data
   is processed, taking into account the capabilities of the libraries in use.

Git-on creates private native Git object files, pack indexes, and regular
diagnostic or transport files on demand. It uses existing filesystems for
these files and removes them with the invocation. Internal synchronization
and recovery may use directory entries, empty locks, and sockets alongside
the writable layer. The external verifier's files follow its native configuration.

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
  construction is excluded. Repeat trials and identify OS page-cache conditions
  where controlled. Remote invocations use independent temporary object stores;
  distinguish a fresh invocation from repeated access within the same run.
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
- Visit more content than decoded memory-cache budgets and verify eviction
  without re-fetching objects already available in the run; do
  not count unavoidable kernel page cache as a second application-owned payload.
- Run the existing immutable-source, ignore-policy, lifecycle, and native-mount
  integration checks after changing the loading architecture. Performance must
  not weaken these semantics.

A benchmark fixture that reuses Git objects must disclose this: it measures
logical namespace growth but does not by itself model a million distinct blobs.
The [pre-merge CI coverage map](ci-performance.md) distinguishes complete
small/medium reads, a million-path namespace traversal with sampled payloads,
and the unchanged million-distinct-file lazy requirement above. Stress fixture
counts are test parameters, not product limits; smaller full reads must never
be reported as full million-file reads.

## Review regressions and bounded randomized stress

Every behavior defect found during review must gain a focused regression test
that reproduces the defect before its fix and passes afterward. Preserve the
trigger and assert the corrected observable result, including failed-operation
postconditions. Test and documentation defects need an appropriate guard or
targeted check too. For nondeterministic races, retain the observed failure and
explain the controlled regression strategy; never
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

The implementation uses a shared pinned object store, demand-only metadata/blob
access and deferred inode creation. A byte-budgeted LRU retains complete small
objects and individual blocks of spilled files. A separate LRU holds whole hot
blobs. Trusted descriptors, block hashes and parsed directories remain in RAM
outside the block budget. Extra objects from reusable packs retain their
verified bytes in RAM until use. Parsing a demanded tree releases its raw
bytes. The FUSE layer retains no duplicate snapshot payloads. Source
errors are reported when the relevant operation reaches them: submodules fail
metadata access, LFS pointers fail content reads, and unrelated entries remain
usable. Listing a directory may report a submodule name without following it.

Local unit tests exercise demand counters, unvisited missing objects, cache
retention/eviction, policy isolation, inode lifetimes, and active large-file
spooling. Real-FUSE regression and million-file performance results must be
verified on the exact published revision in GitHub Actions. Consult the PR's
linked run for that verification; local tests alone are not a real-mount pass.
