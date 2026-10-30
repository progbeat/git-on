# Pre-merge CI cost and coverage

This changes test scheduling and fixtures, not product size limits or execution
semantics. Important validation remains on pull requests and gates the existing
required `package` check. Base checks always run; affected scale/stress/resource
checks must finish before merge. No correctness suite is postponed until after
merge or release.

## Measured starting point

[CI 40883051673](https://github.com/progbeat/git-on/actions/runs/40883051673),
head `2062eb8`, took 76m38s wall time and 586.7 summed job-minutes. Queue delay
and OS caches are not controlled. A skipped release job contributes zero cost.

| Cost | Observed before |
| --- | ---: |
| Real-FUSE job | 76m34s |
| Enabled-passthrough step inside that job | 71m42s |
| Differential stress, two seeds / default ignored | 19s |
| Rust job | 10m48s |
| Historical binary builds/replays and pack scaling inside Rust | 4m13s |
| Million-file full reads | 42 invocations |
| Million-file eager materializations, including no-op/sparse children | 36 invocations |
| FUSE/passthrough million-file child work | 285.6 summed minutes |
| Million-file materialization before child | 51.7 summed minutes |
| Million-file materialized runs after child | 87.2 summed minutes |

The full traversal fixture already shared **nine Git objects**, including one
eight-byte leaf payload. Git deduplication did not eliminate a million physical
worktree entries, FUSE operations or their deletion. Identical contents alone
are not the proposed optimization.

A representative Linux FUSE cell spent 63.3s cloning its million-file baseline,
18.1s running the direct full-read command, and approximately 137.8s deleting
that clone. The cleanup estimate comes from timestamped log gaps after
subtracting measured runner time; it is not an isolated syscall timer. A Linux
remote-materialized cell spent 363.3s preparing twelve million-file snapshots
and 1548.8s after their children. On Mac, one remote signed cell spent 860.8s
preparing six snapshots, 640.0s in children and 989.1s afterward.

New JSONL records explicitly time fixture generation, clone, clone deletion and
fixture cleanup. Each runner separates startup, child work and time after child
completion. A sampled file records `st_size` and `st_blocks * 512`, alongside
`statvfs` fragment size. A local Linux sample was 8 logical bytes / 4096 allocated
bytes; this is an observation on that filesystem, not a universal 4 KiB-per-file
assumption. Filesystem-space deltas are host-wide and can include unrelated I/O.

## Two levels, with a stable required gate

The base runs for every PR: formatting, strict clippy, Rust/unit/protocol tests,
MSRV, ordinary functional/transport/signature/job-control and native Linux/Mac
package/installer checks. Actual-kernel passthrough functional/lifecycle checks
remain in the base as well.

`scripts/ci_plan.py` computes the whole PR diff against its base, rather than
only the most recent follow-up commit. For master pushes it compares the push's
before SHA; unavailable comparisons conservatively require all affected suites.

| Changed input | Additional coverage required before merge |
| --- | --- |
| Runtime, dependencies, build settings, workflows, shared fixtures/helpers, normative contract documents, unknown paths | Scale, differential stress, lazy benchmarks and distinct-object resource controls |
| `scalability.py` or its own harness tests | Scale, including the enabled-passthrough scale groups |
| `stress.py` | Differential stress |
| `benchmark.py` / benchmark contract | Lazy benchmarks and remote million-file demand control |
| `pack_scaling.py` | Distinct-object pack resources |
| Recognized non-contract prose/media | Base only |

Superseded runs for the same PR may be cancelled; master and manual runs use
unique concurrency groups so later pushes do not alter publication semantics.
The `changes` job publishes the decision and exact scenario matrix as a CI-plan
artifact. Workflows are never path-filtered away. Optional jobs use job/step
conditions and may report skipped only when the plan says they are unnecessary.
The existing required `package` aggregate uses `always()`, waits for every base
and conditional job, validates strict boolean requirements and step outcomes,
and reads the actual coverage artifacts. A green job with missing groups,
scenarios, stress operations, object sizes or lazy samples is not sufficient.
A failed classifier, required skipped job, empty output or stale-head report
fails the aggregate. No branch-protection setting or permission is changed.

An explicit `workflow_dispatch` always enables all additional suites. Its
`full_matrix=true` default selects `exhaustive`: full million-file
reads/materialization across the complete interaction matrix, including repeated
remote invocations. `full_matrix=false` selects the optimized affected plan.
Manual runs do not create tags or releases. For example:

```sh
gh workflow run ci.yml --ref RELEASE_CANDIDATE_BRANCH -f full_matrix=true
```

GitHub requires the manual workflow to exist on the default branch; use the
branch selector or `--ref` for the candidate, then inspect the emitted plan and
results before relying on that run. See [GitHub's manual workflow instructions](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/manually-run-a-workflow).
Skipped jobs alone count as success on GitHub, which is why the independent
aggregate validates required coverage rather than trusting a skipped check.
See [GitHub job condition semantics](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/control-jobs-with-conditions).

The measured optimization comparison uses an **all-affected optimized run**,
not a docs-only skipped run. Conditional scheduling alone does not establish a
10x speedup. The fast aggregate's missing/failed/skipped cases are separately
covered by unit tests; a manual exhaustive execution is not claimed unless it
actually runs.

## Coverage mapping

| Invariant / distinct fault | Before | Revised pre-merge check |
| --- | --- | --- |
| Source/policy/signature interactions | 14 physical jobs, 24 logical combinations, every size/workload/repetition | Same 24 combinations at 1k/10k in four grouped jobs when scale is affected; separate source invocations and fresh snapshots |
| Full bytes, complete names, stat and repeated reads | Full million-file read in each combination; separate sparse/no-op preparations | Complete 1k/10k reads in every combination; each full child also stats/reads one path 100 times; selective signed 100k full reads on local/remote Linux FUSE, Linux/Mac materialization and enabled passthrough |
| Huge lazy startup and selected demand | Separate million-distinct-file fixture and benchmarks | Unchanged million-distinct-file true/cat and unused-subtree/large-range gates through real FUSE |
| Full million-path namespace and cleanup | Million payload reads mixed with enumeration | Separate real HTTPS/FUSE million-path traversal, every directory/name checked; one sampled payload per leaf directory plus repeated selected reads. **This is not a full million-payload read.** |
| Eager materialization | Same preparation repeated for no-op, sparse and full children | Full child records startup, then exercises stat/repeated reads and complete bytes in one snapshot. Inert-child/fallback/interrupt semantics remain in `materialized.py` and remote integration tests |
| Fresh remote invocations / no global cache | Repeat every workload after every source/size/signature case | Permanent two-process no-cache regression; each matrix invocation asserts its exact demanded OID set. Verified/unverified invocations also use fresh private ODBs |
| Memory-cache eviction / descriptor growth | 192 and 512 distinct-object controls plus shared-object namespace tests | Same reduced-RLIMIT 192-object test, same-run reread after memory eviction, and 32/128/512 distinct pack controls |
| Streaming beyond the 8 MiB cache/import threshold | Three 96 MiB remote objects, previously chosen to exceed a removed global 256 MiB cache | Three 16 MiB remote objects still exceed both thresholds; local first-range 16/64/128 MiB scaling remains unchanged |
| Whole-invocation overhead and native import writes | Loader-only pack metadata trace could not establish total cost | Optimized CLI: 160 distinct 8 KiB blobs through real FUSE; 32 through materialization, with a separate descendant-following file-I/O trace. Both compare complete clone + identical two-pass closed-read/stat command + cleanup. Exact-head evidence, native decoder writes and unique-load reuse are gated; no timing SLA is invented |
| Long clean unmount / escaped users / cancellation | Full scale plus permanent helper and real-mount regressions | Real 4s-helper regression, 100k full-read teardown, million namespace cleanup and all busy/escaped-reader/signal tests remain. The old million opened-file pressure is not claimed for smaller full reads |
| Write permissions, bytes/links/metadata, protocol errors and lifecycle | Unit/protocol, real mount, packages and seeded differential suites | All retained; differential stress parameters remain two seeds in default ignored, 400 operations, 64 nodes and 64 KiB per file |
| Feature-disabled versus instrumented builds | All Rust tests in both configs, plus Mac | Full normal suite; instrumented suite skips only the already-run expensive pure pattern oracle, with no metrics branches. All snapshot/protocol/runtime tests remain; Mac runs its complete instrumented suite |
| Reproducing known pre-fix failures | Rebuild several immutable historical executables every push | Red-before evidence remains recorded and replay CLI options remain available. Current permanent regressions run every PR; rebuilding known failing binaries is not required to detect regressions in current code |

The pattern oracle was measured separately: 37.5s locally; the wildmatch module
was 3.9s. Only the expensive duplicate oracle is omitted from the second Linux
configuration. Its default Linux run and complete Mac run remain. Strict clippy
still compiles all targets in both configurations, and MSRV/native packaging
checks remain.

## Fixture and execution plan

`tests/scalability.py --profile matrix` uses 1k/10k behavior cases and selected
100k full reads. Wide numeric fanouts reduce directory/path overhead. It uses up
to sixteen distinct eight-byte payloads and two alternating subtree variants,
so full reads verify varied bytes, not just one identical payload. A 100k fixture
has 21 distinct Git objects; a million-path namespace fixture has 23. This is a
namespace model, separate from the million genuinely distinct seven-byte blobs
and the growing distinct-demand resource controls.

The child checks exact per-directory name sets and counts, payload bytes,
within-run repetition, source isolation and fresh generated state. Each run has
its own temporary repository and writes. Only read-only baseline work and the
immutable source fixture are reused within a source group. Passthrough's direct
baseline performs the same generated-write command as its runner case.

The default plan has 97 runner invocations: 66 ordinary FUSE, 20 materialized,
10 enabled passthrough and one million-path namespace run. There are eight
100k complete reads, 28 1k complete reads, and 28 10k complete reads; the namespace
run visits one million leaf names and samples 10,000 leaf payloads, in addition
to the pinned ignore file and 100 repeated selected reads. No default full read
is called a million-file full read. `--profile exhaustive` remains available for
explicit diagnostics; it is not a deferred correctness gate.

The private source probe can report metrics without enumerating the ODB after
every read, then explicitly enumerate and verify the full available OID set
once at the end. The 512-object control fell from 126.6s to 23.1s on the local
executor. This measures test overhead, not product throughput; historical probe
wall times include their per-read enumeration.

The target is at least 10x lower wall time and substantially less runner time.
The revised full CI run and PR description record measured results against the
76m38s / 586.7-minute baseline. These local controls do not establish that target
by themselves, and the target is not a product performance SLA.
