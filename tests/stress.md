# Seeded real-FUSE filesystem stress

`stress.py` runs an actual `git-on <tree> -- python ...` command against a real
FUSE mount. It executes a bounded, seeded stream of filesystem operations and
compares it with a separate host-directory oracle. A missing `/dev/fuse`, missing
mount helper, failed mount, operation mismatch, timeout, leaked runtime directory,
or leaked Git pin is a failure. There is no emulated pass or implicit skip.

## CI and local use

Build the binary first, then on a Linux host with functioning FUSE:

```sh
python3 tests/stress.py --binary target/debug/git-on --require-fuse \
  --seed 0x5eed --seed 0xc0ffee --operations 400 \
  --max-nodes 64 --max-file-bytes 65536 --timeout 90 \
  --trace-dir stress-artifacts
```

Each seed gets a fresh Git repository, independent oracle, and fresh mount. The
same binary under review runs the mount; there is no test-only filesystem
implementation in this path. `--require-fuse` is accepted for CI clarity, but FUSE
is required even without the flag. CI should preserve `stress-artifacts/*.jsonl`
when a run fails. Seeds, per-operation counts, exact error counts, and elapsed
time are printed. Use several fixed seeds for repeatable acceptance and add
other explicit seeds for exploration. Seed replay requires the same harness
version; operation trace replay avoids dependence on the random generator.

## Oracle and workload

Every operation checks its exact return value or exact errno, then compares the
entire namespace, regular-file bytes, symlink targets, inode kinds, permission
bits, ownership, non-directory sizes/link counts, mutable-directory link counts,
and hard-link equivalence classes. Numeric inode IDs, timestamps, block counts,
and directory byte sizes are host-specific and are not compared. Tracked
directory link counts are excluded because their snapshot metadata is immutable
while ignored children may be added. Mutable-directory link counts are checked
against the host, including after mkdir, rmdir, and directory rename.

The initial fixture contains tracked files, executable bits, a tracked symlink,
nested ignore rules, explicit negation, and a tracked file inside an ignored
directory. The oracle implements only this small, fixed ignore grammar,
independently of the product's matcher. Other Git pattern semantics are covered
by the integration suite. Policy denials expect EROFS exactly. All ordinary
filesystem failures use the exact errno from the host operation; broad error
families do not count as success.

A deterministic prelude covers sparse writes around 4096 bytes, shrinking and
extending files, append, mutable hard-link aliases, symlinks to mutable and
protected files, dangling symlinks, populated-directory moves, wrong-type
operations, collisions, missing paths, and denied modifications/rename/unlink/
hard links for tracked entries. Runtime-created `.gitignore` cannot grant new
writes. Conflicting dirty-worktree and `.git/info/exclude` rules cannot override
the selected tree. The full snapshot comparison checks rejected operations for
partial mutation as well as checking success.

After the prelude, choices are genuinely seeded and state-dependent: create,
write, append, truncate, read, rename, unlink, mkdir, rmdir, hardlink, symlink,
stat, lstat, listdir, and policy probes are interleaved. Existing paths, aliases,
colliding names, missing names, empty data, same-source/destination rename,
negated names, non-ASCII/space/dot names, and offsets near common block boundaries
are sampled. Mutation can use ignored root files, nested ignored files, and
ignored descendants. Each next operation is selected from the current oracle
state, not from the observed mount, so product defects cannot steer the oracle.

The stream is deliberately sequential: concurrency would introduce scheduling
ambiguity into exact replay and the per-operation oracle. Concurrent real-FUSE
behavior has separate integration regressions; this harness does not claim
concurrent stress or exhaustive operation/protocol coverage.

## Bounds and evidence

Defaults are 400 operations per seed, at most 64 mutable directory entries, at
most 64 KiB per file, and a 90-second child deadline. Each syscall write payload
is at most 1024 bytes. The outer runner enforces another deadline and attempts
TERM then KILL on a timeout. CLI limits permit up to 10,000 operations, 256
entries, 1 MiB per file, and 600 seconds. All limits are test-harness limits,
not product disk quotas or performance promises. Full state inspection after
every operation makes this a correctness stress test, not a throughput benchmark.

These bounds limit logical file content (including sparse holes). Hard links
share an inode. Trace operations include write bytes and resolved operation
parameters; large read results are hashed only for compact journaling, after
exact raw-byte comparison. Traces contain only generated fixture data.

## Failure reproduction and regression

An operation is written and flushed to the JSONL trace *before* either filesystem
is touched. Failures identify the seed, operation index, expected/actual result,
and the first differing paths. The trace includes the failing operation even
when it crashes or hangs before returning; this is a replayable operation
sequence, not just a seed or a crash count.

```sh
python3 tests/stress.py --binary target/debug/git-on \
  --replay stress-artifacts/seed-24301.jsonl --trace-dir replay-artifacts
```

Replay restores the same fixture and limits, executes the recorded operations
in order, and writes a new trace. It does not copy the expected results from the
old run: the independent oracle recomputes them. A partial trace can reproduce a
failing prefix. Never overwrite the input trace. Traces are versioned; preserve
the harness revision together with failing traces. A confirmed defect should
also receive a reduced named integration/protocol regression before its fix is
claimed verified.

For machines without FUSE:

```sh
python3 tests/stress.py --self-test
```

This is explicitly **host-only harness validation**, never product/FUSE evidence.
It validates the fixed policy grammar, rejects unsafe replay paths, checks that
wrong errno and injected content/namespace/metadata/link corruption are detected,
and runs seeded host-oracle streams, exact trace replay, and seed regeneration.
The host-mode policy adapter exists solely in these self-tests and is never used
by a normal stress run.
