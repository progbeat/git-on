# Pre-merge CI cost and coverage

This changes test scheduling and fixtures, not product size limits or execution
semantics. Important validation remains on pull requests and gates the existing
required `package` check. Base checks always run; affected scale/stress/resource
checks must finish before merge. No correctness suite is postponed until after
merge or release.

The native Apple Silicon package check uses `macos-15`. GitHub's
[macOS 14 retirement schedule](https://github.com/actions/runner-images/issues/13518)
cancelled its previous runner before any build step during the October 31
brownout, and removes that image on November 2, 2026. The supported replacement
keeps the same package target, extraction/installer/behavior checks and
`MACOSX_DEPLOYMENT_TARGET=13.0`; it does not raise the product's minimum OS.

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
| Source/policy/signature interactions | 14 physical jobs, 24 logical combinations, every size/workload/repetition | Same 24 combinations at 1k in four grouped jobs when scale is affected; separate source invocations and fresh snapshots |
| Full bytes, complete names, stat and repeated reads | Full million-file read in each combination; separate sparse/no-op preparations | Complete 1k reads in every interaction; each full child also stats/reads one path 100 times. A separate pinned WebKit local FUSE case reads every path/blob twice |
| Huge lazy startup and selected demand | Separate million-distinct-file fixture and benchmarks | Unchanged million-distinct-file true/cat and unused-subtree/large-range gates through real FUSE |
| Large namespace, diverse real payloads and cleanup | Shared-object million-path traversal plus selective 100k reads | WebKit full namespace and payload validation; no separate million-path count checkbox. Small synthetic namespace/corruption regressions remain |
| Eager materialization | Same preparation repeated for no-op, sparse and full children | Full child records startup, then exercises stat/repeated reads and complete bytes in one snapshot. Inert-child/fallback/interrupt semantics remain in `materialized.py` and remote integration tests |
| Fresh remote invocations / no global cache | Repeat every workload after every source/size/signature case | Permanent two-process no-cache regression; each matrix invocation asserts its exact demanded OID set. Verified/unverified invocations also use fresh private ODBs |
| Memory-cache eviction / descriptor growth | 192 and 512 distinct-object controls plus shared-object namespace tests | Same reduced-RLIMIT 192-object test, same-run reread after memory eviction, and 32/128/512 distinct pack controls |
| Streaming large-object and import thresholds | Three 96 MiB remote objects, previously chosen to exceed a removed global 256 MiB cache | Three 16 MiB objects exceed the 64 KiB raw streaming threshold and 8 MiB native import threshold; their combined working set exceeds the 16 MiB payload budget. Local 16/64/128 MiB range controls remain |
| Whole-invocation overhead and native import writes | Loader-only pack metadata trace could not establish total cost | Optimized CLI: 160 distinct 8 KiB blobs through real FUSE; 32 through materialization, with a separate descendant-following file-I/O trace. Both compare complete clone + identical two-pass closed-read/stat command + cleanup. Exact-head evidence, native decoder writes and unique-load reuse are gated; no timing SLA is invented |
| Long clean unmount / escaped users / cancellation | Full scale plus permanent helper and real-mount regressions | Real 4s-helper regression, WebKit teardown and all busy/escaped-reader/signal tests remain. Generated million opened-file pressure is not claimed for the replacement |
| Write permissions, bytes/links/metadata, protocol errors and lifecycle | Unit/protocol, real mount, packages and seeded differential suites | All retained; differential stress parameters remain two seeds in default ignored, 400 operations, 64 nodes and 64 KiB per file |
| Feature-disabled versus instrumented builds | All Rust tests in both configs, plus Mac | Full normal suite; instrumented suite skips only the already-run expensive pure pattern oracle, with no metrics branches. All snapshot/protocol/runtime tests remain; Mac runs its complete instrumented suite |
| Reproducing known pre-fix failures | Rebuild several immutable historical executables every push | Red-before evidence remains recorded and replay CLI options remain available. Current permanent regressions run every PR; rebuilding known failing binaries is not required to detect regressions in current code |

The pattern oracle was measured separately: 37.5s locally; the wildmatch module
was 3.9s. Only the expensive duplicate oracle is omitted from the second Linux
configuration. Its default Linux run and complete Mac run remain. Strict clippy
still compiles all targets in both configurations, and MSRV/native packaging
checks remain.

## Fixture and execution plan

`tests/scalability.py --profile matrix` now uses 1k behavior cases. Wide numeric
fanouts reduce directory/path overhead. It uses up
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

The synthetic matrix has 60 runner invocations: 48 ordinary FUSE, eight
materialized and four enabled passthrough. All 24 source/backend/policy/signature
interactions remain; 28 full children validate complete names/bytes plus repeated
access. Compared with the preceding 97-case optimized plan, 28 repeated 10k full
reads, eight selected shared-object 100k reads and one shared-object million-path
namespace run are replaced by the real fixture below. `--profile exhaustive`
and explicit namespace diagnostics remain available but are not deferred gates.

`webkit-scale` is required whenever scale is affected. It acquires exactly one
WebKit source at `4b125afe35a2e95bf764d7a5e2efe7dc574a0653`, checks root tree
`be3bad9ff779597380c7ba741ff250ef8a14225f` and native object integrity, and verifies
472,895 blob paths / 5,522,313,781 path-summed bytes / zero gitlinks. Path counts,
unique-object counts and byte volumes are separate. One real local readonly FUSE
invocation validates the complete namespace, modes, symlinks and contents twice.
Every source blob OID is checked by the child; empty paths remain in namespace/
byte checks but are not falsely required to trigger payload decoding.

The useful bounded workload runs 32 upstream `webkitcorepy` tests for Version,
string utilities and filtered calls through the standard Python unittest loader.
These are modules in test-webkitpy's suite. Its ordinary entrypoint invokes
AutoInstall.install_everything even for a selected subset, so the lightweight
path uses the supported DISABLE_WEBKITCOREPY_AUTOINSTALLER=1 switch, -B and -S,
without the browser build, SDK, third-party site packages or mass installation.
Three fresh local and three file:// invocations each run two fresh Python test
processes in one mount, retaining the real test output and rejecting failures,
skips or a wrong test count. Full/controlled resource cases independently enforce
exact decode reuse; test invocations record their actual loaded/received data.
Command N is not inferred from preparation counters. This does not claim a full remote
WebKit read, full Mac materialization or public-network cold-source performance.

One full native materialization per source is reused for independent direct
command trials. Each reported complete baseline composes the measured same-source
clone/checkout, identical direct command and complete cleanup; reused setup and
cleanup are explicitly marked as one sample, not independent clone trials.
Local native clone requests shared objects (native Git may use its transport for
a shallow source); remote clone uses the same file:// transport. Only the disposable baseline's attributes disable
checkout transformations to match raw Git blobs. No source/caller configuration
is changed. LFS hydration and external dependencies are outside the fixture;
unsupported pointers are not silently excluded from full reads.

CPU accounting includes waited native descendants. A wait4 helper records the
largest waited process's peak RSS, including descendants; it is not an aggregate
physical-memory peak. Additional 20 ms process-tree RSS sums are non-atomic,
count shared pages twice and can miss fast/protected descendants. Partial tree
observations and independent descriptor denials remain explicit. Missing primary
wait4 observations fail the gate, rather than becoming zero overhead. This
replaces the first WebKit gate's invalid demand for complete /proc visibility;
no process-access or security setting is changed.
Received session-pack bytes are exact compressed Git payloads; native received
packs may have been normalized by index-pack. Neither includes protocol/TLS wire
framing. Decoded verified blob bytes are reported separately. These observations
do not establish a fixed total-RAM bound or a formal whole-program complexity
proof. All invocation preparation, command execution and cleanup stay in time.

`package` checks the WebKit job and its actual exact-head reports, including both
full passes, all six unit-test invocations, process resources, transfer counters and
complete baseline components. Missing/stale/duplicate/incomplete reports fail.
The pinned run must pass before it is cited as evidence; adding this job alone
does not establish WebKit acceptance or #20 completion.

The private source probe can report metrics without enumerating the ODB after
every read, then explicitly enumerate and verify the full available OID set
once at the end. The 512-object control fell from 126.6s to 23.1s on the local
executor. This measures test overhead, not product throughput; historical probe
wall times include their per-read enumeration.

The target is at least 10x lower wall time and substantially less runner time.
The preceding all-affected optimized run at `a3d345f`, before this WebKit change,
passed in 6m13s / 31.8 job-minutes against the 76m38s / 586.7-minute historical
baseline: 12.3x wall and 18.4x job-time reductions. Its 97-case synthetic evidence
is historical, not a measurement of this replacement. The WebKit-inclusive
plan needs its own measured final-head run. CI optimization is not a product
performance SLA or proof of the complete overhead contract.


The WebKit-inclusive all-affected run at `9010d4c`, [CI 41183217607](https://github.com/progbeat/git-on/actions/runs/41183217607),
passed in **16m25s / 40.8 summed job-minutes**: 4.7x wall and 14.4x job-time
reductions versus the historical starting point. The 10x wall-time target is
therefore **not met by this plan**. WebKit took 15m57s including source/build,
complete materialization baselines and both full-read passes. The useful
32-test invocations themselves took under one second. This separates the cost
of meaningful pre-merge real-repository validation from the earlier synthetic
speedup; no expensive legacy full matrix was rerun or silently moved post-merge.


For full-path attribution, the current diagnostic stage uses `--profile-full`
on the existing single WebKit full-read invocation. The workload, baseline,
number of passes and actual-data gates are unchanged. Results explicitly mark
profiling; inclusive read/open/hash/spill/decode spans overlap and must not be
summed. The private complete invocation timeline additionally separates source,
mount, command, unmount and cleanup and preserves parent-observed total time.
These diagnoses are not uninstrumented timing comparisons. The useful test
trials stay unprofiled, and no second full WebKit run is added within the job.

The same diagnostic command samples up to 16 regular-file paths per size bucket
(<1 KiB, <=64 KiB, <=1 MiB, >1 MiB) after each pass. PROT_NONE mappings and
mincore inspect existing page residency without reading file payloads; raw and
FUSE-view pages are reported separately. Missing/protected observations stay
explicit. Sampling time stays in the diagnostic command/total time. CPU, RAM
limit and filesystem metadata use the existing benchmark collector; system and
cgroup file/anonymous-cache observations accompany the samples. These are
limited observations, not a whole-cache census or a physical-memory peak.
