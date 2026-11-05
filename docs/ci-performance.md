# Pre-merge CI cost and coverage

The required `package` check validates job outcomes and exact-head evidence.
Base checks always run. Affected scale, stress, lazy-access and resource checks
must finish before merge; a skipped real FUSE check is not a pass.

## Selection and execution

`scripts/ci_plan.py` selects checks from changed paths. Runtime, dependency,
workflow, shared-helper and contract changes select all heavy suites. Only
independent test drivers may select an individual suite. Ordinary prose/media
changes keep the base checks. Unknown paths select all heavy suites.

Base coverage includes formatting, strict clippy, Rust tests and MSRV, the FUSE
coverage guard, release policy, real Linux FUSE, macOS CLI/materialization and
native Linux/macOS ARM packages and installers. The native ARM package runner
uses `macos-15`; other macOS behavior uses `macos-26`.

The ordinary synthetic scale matrix exercises all source/backend/write-policy/
signature interactions at 1,000 paths. Full children check complete directory
membership, varied payloads, stat, repeated reads, source isolation, generated
writes and cleanup. Passthrough uses actual kernel capability checks and remains
in `real-fuse`. `--profile exhaustive` is an explicit diagnostic option.

Separate lazy-access checks cover million-object demand, untouched subtrees,
random reads, repeat access and large-file ranges. Resource checks cover growing
pack sets, decoded-object reuse, metadata reuse, kernel-cache behavior and
reduced descriptor limits. Wire, mounted and storage-corruption tests protect
their distinct contracts.

## Small command on public WebKit

`webkit-scale` runs `tests/real_repository.py run` through real Linux FUSE against
public HTTPS at WebKit commit `4b125afe35a2e95bf764d7a5e2efe7dc574a0653`.
It reuses the instrumented release binary from the same run's required
`real-fuse` job; it does not repeat compilation or use a cached older binary.
The pinned revision contains 472,895 blob paths and 5,522,313,781 logical payload
bytes. These describe the fixed source; CI does not scan it to recount them.
The child checks its actual root tree against
`be3bad9ff779597380c7ba741ff250ef8a14225f`.

One fresh Git-on invocation runs 32 upstream `webkitcorepy` tests for Version,
string utilities and filtered calls. Two fresh Python processes run the same
tests within that mount. No full clone, source fixture, recursive manifest,
checkout or materialization baseline is prepared. The ordinary test entrypoint's
SDK/dependency bootstrap is bypassed using
`DISABLE_WEBKITCOREPY_AUTOINSTALLER=1`, `-B` and `-S`.

The gate bounds the pinned workload's demand: at most 32 decoded trees, 4,096
prepared entries, 128 blob/header loads, 1 MiB decoded payload and 1 MiB received
Git pack payload. It requires real FUSE reads, matching imported/loaded payloads
across the two passes, correct upstream output without skips, and complete
invocation timing and waited descendant CPU accounting. These budgets constrain
the test's small command; they are not repository or product size limits.
The shared integration-command timeout also makes stalled public-network access
fail rather than occupy a runner indefinitely.

The job summary displays complete invocation wall time, decoded object count,
payload bytes and received pack bytes next to the pinned source size. Time
includes network access, all subprocesses, the command, unmount and cleanup.
Received pack bytes exclude protocol/TLS framing. Invocation storage starts
empty; network and OS caches are uncontrolled. The report is checked again by
`package`, which rejects missing, duplicate, stale, dirty or eager-work evidence.

This check demonstrates a small real command on a large public source. It does
not measure full-tree throughput, a clone speed ratio, a fixed total-RSS bound
or the complete performance contract in #20. That issue remains open.
