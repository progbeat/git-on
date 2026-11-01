# Bounded FUSE syscall diagnostic

Run `tests/fuse_syscall_profile.py` directly on an existing Linux FUSE host before
merge. It needs no default-branch change or separate PR. Coordinate one active
profiling invocation on the selected host rather than starting duplicate runs.

The optional `Bounded FUSE profile` workflow is manual-only for later standalone
reproduction. GitHub's default-branch registration prerequisite applies only to
the first dispatch of that optional workflow, not to profiling itself. Existing
required CI and branch protection remain unchanged. This diagnostic is included
in the single 1.4.0 implementation PR.

Profile an ordinary release build of commit
`5f7abb436c28e9851c2cae192e4126fceb7d3d19` on one CPU, using a disposable local
packed source with 8,192 distinct 4 KiB blobs (32 MiB). This exceeds the subject's
16 MiB data LRU. It does not force kernel page-cache misses. Source caches are
warm/uncontrolled, and no cache-dropping or cache-advice operation runs.

Use separate clean checkouts for the driver and profiling subject. With the
driver checkout at `DIAGNOSTICS`, subject checkout at `SUBJECT` and an ordinary
release binary built from `SUBJECT`, the invocation is:

```sh
taskset -c CPU nice -n 15 ionice -c 3 python3 DIAGNOSTICS/tests/fuse_syscall_profile.py \
  --checkout SUBJECT --binary SUBJECT/target/release/git-on \
  --work NEW_FIXTURE_DIRECTORY --output NEW_EVIDENCE_DIRECTORY \
  --files 8192 --bytes 4096
```

The subject must be exactly `5f7abb436c28e9851c2cae192e4126fceb7d3d19`; the driver
refuses a different or dirty subject checkout. Record both checkouts' commits and
the binary hash. Diagnostic-only consolidation does not change that subject's
runtime inputs. Keep the work and evidence directories outside the subject.

The seven cases are two untraced full Git-on invocations, two complete shared-clone
plus identical-command plus cleanup baselines, and separate server, requester and
command syscall profiles. Every child checks all stat sizes and payload bytes on
first and repeated reads. Timed cases are fresh invocations of the same revision.
The fixture is prepared once outside the measured invocation and baseline times.

The launcher starts Git-on without tracing, waits until the command confirms its
actual FUSE mount, and only then becomes `strace -f -c`. It stays an ancestor of
the tracee. Existing server threads must all report the tracer before workload
release. The tracer detaches before the command exits and unmount begins.
The supervisor uses process-local subreaping to collect the server after the
launcher exits; this does not alter host ptrace, Yama, perf or capability settings.
No system packages are installed. Missing tools or an attach denial stop the run
and retain partial evidence; the workflow does not retry.

Interpret the measurements separately:

- `plain-*` includes startup, mount, command, unmount and cleanup, plus the declared
  diagnostic supervisor/launcher. It is a bounded control, not WebKit acceptance.
- `server_and_waited_children` is waited CPU for Git-on and its reaped descendants;
  the payload CPU is included, so do not add it again. Largest waited process RSS
  is not aggregate physical memory.
- `server_work_own_cpu_tick_delta` is the server's own thread-group user/system
  CPU while the command does its reads, at `/proc` scheduler-tick resolution.
- Child first/repeat wall and user/system CPU are separate. Their difference is
  not a measurement of device I/O wait.
- Server `strace` covers existing threads and future descendants while mounted;
  the already-running payload is profiled in a separate command case.
- Requester `strace -P /proc/CHILD/status` estimates system CPU for those file
  operations; it does not isolate user CPU for the Rust status parser.
- Traced barriers and ptrace change elapsed time. Syscall system-time estimates
  are not untraced component timings; independent case totals are not additive.
- Cleanup is included in the untraced total and waited CPU, but its syscalls are
  outside the late-attach window.

For local supervision checks on a host without FUSE, the driver has an explicit
`--allow-materialized` flag. Such output says `SELF-TEST ONLY` and must not be
reported as a real FUSE profile. Normal workflow execution never enables it.

Each payload has a 30-second supervised workload deadline. Readiness, detach and
cleanup have separate finite deadlines; the job has a 20-minute bound including
the one-CPU release build. On success, refs, exit statuses, mount absence and the
empty owned invocation directory are checked before deleting the disposable
fixture. On failure, evidence and recoverable state remain for diagnosis rather
than recursively deleting a possibly mounted tree.
