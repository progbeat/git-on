# git-on behavioral and performance specification

This document defines required behavior. The implementation status below is
separate from that contract; a requirement is not a claim of a passing test.

## Command and revision

- `git on [--keep] <tree-ish> <command> [args...]` resolves the selected Git tree once and
  launches the given argv from its root without introducing a shell.
- The selected tree, not the index or working directory, determines source
  contents throughout the run. Later changes to refs do not change that tree.
- The command remains an ordinary program: standard streams, filesystem errors,
  and inherited environment retain their documented meanings. stdin, stdout,
  and stderr are inherited directly; git-on does not capture or tee output.
- Only `GIT_ON_TREE_OID` is added or overridden in the child environment. No run
  ID, output-directory, or run-directory variable is synthesized or consumed.
  Caller-provided variables with such names remain ordinarily inherited.
- Runner flags are parsed only before the tree-ish. Everything after the
  tree-ish is literal command argv, including an argument spelled `--keep`.
- git-on has no built-in execution logs, durable run manifests, artifact export,
  or experiment-recording layer. Callers may record output externally.

## Filesystem policy

- Entries present in the selected tree are immutable. An ignore match does not
  permit changing, replacing, removing, renaming, or shadowing tracked entries.
- New paths are writable only when ignored by applicable `.gitignore` files in
  the selected tree, using Git's nested, negated, anchored, and ignored-parent
  semantics. Host-global rules, `.git/info/exclude`, working-tree edits, and new
  runtime `.gitignore` files do not alter this policy.
- The same rule governs all programs and languages. No Python, uv, Numba, cache,
  or environment-directory exception exists.
- A disallowed filesystem operation returns an appropriate ordinary filesystem
  error to the caller. Denial alone does not make git-on kill the application.
  The application may handle the error, continue, or exit normally.
- The writable layer is fresh for each run. It uses private filesystem-backed
  storage under the system temporary location and respects `TMPDIR`; git-on
  creates no private memory filesystem and imposes no disk quota. The underlying
  temporary filesystem's capacity and operating-system limits still apply.
- Default completion unmounts and deletes the writable layer. `--keep` unmounts
  but preserves that backing directory and reports the containing temporary
  directory on stderr; retained named files are under `backing/`. It does
  not retain a mount, duplicate untouched source files, or reuse those writes in
  another invocation. Host paths explicitly written by the command are outside
  this cleanup scope.
- Abrupt process or machine termination and busy mounts may require stale-run
  cleanup. Minimal temporary recovery metadata is permitted for mount identity,
  locking, and releasing the exact Git pin; it is not a run manifest or log.
  `git on --cleanup <temporary-directory>` must refuse active or deliberately
  retained directories, avoid unrelated mounts and changed pin targets, and
  never terminate processes based on stale PIDs.
- The filesystem is not a host security sandbox. This contract does not promise
  isolation from external files, processes, networks, or inherited credentials.

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
  `git on <tree-OID> cat <digit-path>`. Record the seed and chosen number and
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
