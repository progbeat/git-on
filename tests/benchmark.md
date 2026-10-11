# Performance benchmarks

`benchmark.py` is a Python 3.11+ standard-library harness for the real Linux
FUSE runtime. Alongside timings and RSS, it collects the runtime's explicit
snapshot counters and checks structural lazy-access invariants from the
[lazy-access specification](../docs/specification.md). An eager tree walk is a
failed sample even when it happens to finish quickly. The harness does not
emulate, implement, or infer lazy loading.

## Exact million-file benchmark

Requires Git, `cat`, Linux, a usable `/dev/fuse`, and `fusermount3` (or
`fusermount`). Build the instrumented release binary before preparing or timing anything:

```sh
cargo build --release --locked --features test-metrics
python3 tests/benchmark.py self-test
python3 tests/benchmark.py prepare \
  --directory /tmp/git-on-million --digit-files 1000000
python3 tests/benchmark.py run \
  --fixtures /tmp/git-on-million --output benchmark-results \
  --binary target/release/git-on --repeats 3 --warmups 0 \
  --timeout 180 --seed 0
```

Both destination directories must be new. Nothing is checked out. Preparation
uses `git fast-import` to write packed objects, with deterministic contents and
commit metadata. There are exactly 1,000,000 leaf files for `000000` through
`999999`. The first five digits are directories; the sixth is the filename.
For example, `1/2/3/4/5/6` contains exactly `123456\n`, seven bytes. Every file
has distinct content and a distinct blob object. Every directory has ten
entries in the complete million-file fixture; there are 111,111 directories
including the root. No tree-object or blob-object reuse substitutes for those
million distinct contents. The fixture contains 1,111,112 reachable objects:
one million blobs, 111,111 trees, and the fast-import commit.

Preparation verifies every leaf path and its expected blob ID. Generation and
verification are outside every timed sample. The setup itself can take time and
memory, but is separate from runtime measurements. The external harness records
the selected root tree ID and Git's actual object-storage statistics in `fixtures.json`.
This fixture description is not a runtime run manifest.

Before each sample, the harness chooses a number using a deterministic random
seed and records the chosen six-digit number and path. It then runs the compiled
external-command executable directly, equivalently:

```sh
git-on <tree-OID> /usr/bin/cat 1/2/3/4/5/6
```

The actual `cat` path is resolved and recorded. Calling the executable directly
excludes Git's external-command dispatch overhead. There is no Python child or
shell in this workload. The primary metric, `verified_stdout_ms`, starts
immediately before launching git-on and stops when the harness receives the
complete expected seven-byte output. This includes mount/setup, the read, and
external pipe observation. The command inherits stdout directly; git-on does not
capture or forward it. The metric does not wait for runner teardown. A sample is
accepted only after successful exit, verification that the entire stdout is
exactly the chosen number plus newline, and the snapshot-counter and cache-bound
checks below. Any later extra
output invalidates the sample. The external command, path selection, three
repeats, seed 0, and zero warmups above match the original eager baseline;
fixture preparation and verification remain outside timing.

`--digit-files` defaults to 1,000 for a bounded smoke run. A smaller fixture uses
the first N zero-padded numbers at the same six-component depth; not every
directory is full. It is explicitly not the million-file acceptance workload.
For scale comparisons, prepare separate 1,000 / 10,000 / 100,000 / 1,000,000
fixtures and run them with the same release binary and environment.

## Measurements and caveats

The external harness creates `results.jsonl` with metadata, all raw samples
(including warmups), and per-case minimum/median/maximum summaries. It also saves
per-sample stdout, stderr, and raw test-metrics pipe output. git-on creates none
of these files and has no built-in run manifest or log capture. Failures retain
available harness evidence, produce no successful timing summary for that sample, and
stop the run with nonzero status. Missing FUSE fails before taking any sample;
a device existing but unable to mount fails the actual run. There is no
emulation, automatic skip, or silent downscaling.

- `verified_stdout_ms`: external observation described above. Scheduling and
  pipe delivery are part of this measurement.
- `observed_total_ms`: invocation through observed runner exit and stdout EOF,
  including teardown and up to roughly 10 ms of exit-polling delay.
- `wait4_peak_rss_kib`: Linux `wait4`'s `ru_maxrss`, in KiB. This is a maximum
  per-process resident-set high-water, including waited-descendant accounting;
  it is not a sum of simultaneous process-tree RSS. It also does not account
  for all kernel page-cache memory. Small cases can be affected by launcher /
  interpreter and fork accounting floors. The harness's own pre-spawn high-water
  is recorded separately; it is not subtracted or presented as runner memory.
- User/system CPU seconds are Linux `wait4` usage, including accounting from
  waited descendants.
- `child_startup_ms` is deliberately null for unmodified `cat`: it emits no
  separate child-start marker. Receiving correct output is not mislabeled as
  child startup. The secondary Python workloads do provide that measurement.
- `snapshot_metrics.tree_reads`: decoded directory-object fetches through the
  snapshot layer. `header_reads`: blob-size/header fetches through that layer.
  `blob_loads` and `blob_bytes`: source blob fetches and full decoded payload
  bytes before slicing. Revision/tree-ish resolution precedes these counters.
  They do not measure physical disk I/O, compressed pack bytes, or libgit2's
  internal delta-base work. Seven application bytes alone would not establish
  any of these counts.
- `snapshot_metrics.cache_hits`: reuse reported by the snapshot layer. It is not
  an OS/FUSE page-cache hit counter. Blob/tree cache bytes and entry counts are
  the retained state at collection, with explicit configured byte limits; they
  are not peak RSS or all process memory. The harness rejects cache byte counts
  above the corresponding limits and rejects absent or malformed counters.
- The digit workload may fetch at most six tree objects (root plus the five
  directories on its chosen path), at most one blob header, and exactly one
  seven-byte blob. This rejects whole-tree enumeration, sibling metadata scans,
  and unused content loads regardless of timing. These bounds stay the same
  for 1,000 and 1,000,000 distinct leaves. The `scale`/`repeat` cases permit only
  the root and active subtree, three accessed headers, and their three small
  blobs. The `large` case permits only its root, one accessed header, and one
  whole-blob decode for the open file, regardless of how many FUSE range reads
  are needed. Large readers may spool decoded contents to temporary storage;
  cache byte counts describe the shared in-memory cache, not that spool.

All eleven checked counters arrive as one JSON object on a separate pipe opened
by the harness. `Popen(pass_fds=...)` explicitly passes its write descriptor to
the runner, with `GIT_ON_TEST_METRICS_FD` naming that descriptor. The opt-in
`test-metrics` build marks it close-on-exec before launching the workload and
writes counters independently of stdin/stdout/stderr. Normal builds ignore this
test-only setting and have no metrics output channel. The harness records raw
pipe bytes in `.metrics` files and copies the validated counters into sample
records and minimum/median/maximum summaries. Empty, malformed, or missing
metrics fail the sample; the harness never falls back to timing-only evidence.

The test build adds this end-of-run reporting, so record the feature flag when
comparing total runtime and memory. Output latency is observed externally before
teardown reporting. Old uninstrumented baseline results remain timing/RSS
evidence only and cannot retrospectively prove lazy access.

Metadata records CPU model, architecture, CPU count, physical RAM, visible
cgroup memory limit, OS/kernel, Python and Git versions, fixture and evidence filesystem types,
source checkout revision/dirty state, executable version and SHA-256, argv,
fixture layout, and cache policy. Storage-device model is unknown; add host/disk
and load details when comparing machines. The checkout revision alone does not
prove which sources built an arbitrary `--binary`; retain the build log and use
the executable hash to identify what was actually measured.

These are uncontrolled-OS-cache measurements. Fixture construction/verification
already touches the object store. The harness does not flush caches, request
root, or label a first sample “cold.” The exact command above has no warmup;
all three observations are reported. To deliberately warm each case, use
`--warmups 1` (the normal default); these samples are preserved but excluded from
summaries. Each sample starts a fresh git-on process and mount, so process-local
caches are fresh even when the OS page cache is warm. Case order is shuffled
deterministically between rounds. Do not compare an unrecorded debug build with
a release result, or claim an SLA from a single machine's median.

The per-sample timeout first sends TERM to the private measurement session and
then KILL after five seconds. Normal signal handling lets git-on unmount and
clean up; a force kill can leave temporary mount/backing state and a Git pin.
Use `git on --cleanup <stale-temporary-directory>` if recovery is needed. The
harness does not discover or remove unrelated temporary directories. An escaped
descendant retaining a capture descriptor cannot turn a timeout into a success;
incomplete capture is recorded as failure. An external CI timeout, OOM kill, or
machine failure may prevent the harness from writing a final sample. Retain the
available harness evidence and describe that outcome as a failure, not a timing
result. A timeout is a safety limit, not an agreed performance threshold.

## Secondary workloads

The earlier shared-object namespace test and read-focused cases are separate
from the exact digit benchmark:

```sh
python3 tests/benchmark.py prepare \
  --directory /tmp/git-on-secondary --suites scale large \
  --sizes 0,1000,10000,100000,1000000 --large-mib 1,16,64
python3 tests/benchmark.py run \
  --fixtures /tmp/git-on-secondary --output secondary-results \
  --binary target/release/git-on --suites scale repeat large \
  --repeats 5 --warmups 1 --repeat-count 1000
```

- `scale`: read the same three 256-byte files once, while an untouched sibling
  subtree grows. Fanout is at most 100. All unused paths share one 256-byte blob;
  repeated trees are reused. This efficiently stresses logical namespace growth
  without requiring per-path startup allocation, while strongly favoring object caches and
  deduplication. It does not model a million distinct blobs and does not replace
  the digit fixture. The zero-file fixture still has an empty `unused` directory.
- `repeat`: on the zero-unused-file fixture, reopen and read those three files
  1,000 times in a single child by default. This is separate from launch scaling
  and reports the repeated-operation duration. Kernel caching may satisfy reads;
  workload iterations must not be interpreted as object-cache hits. The actual
  runtime object-cache count is recorded separately.
- `large`: read only the first and last 4,096 bytes of a large blob, 8,192 bytes
  total regardless of blob size. Data consists of deterministic SHAKE256 blocks;
  it is distinct from the tiny repeated blob in `scale`. Git may still have to
  decode the entire object. Decoded blob bytes are recorded rather than
  equated with the 8,192 application bytes. This is a bounded-range workload,
  not a full-file throughput test. Integration tests separately verify a 16 MiB
  snapshot read in chunks; unit tests and this instrumented workload verify
  decode reuse.

Secondary fixtures default to loose objects; `prepare --storage packed` repacks
all fixture refs. The primary digit fixture always starts packed via fast-import.
Packing, compression, and shared-object biases must be retained in comparisons.

Secondary children run host Python with `-I -S -c`. Their first timestamp is taken
before accessing the mounted tree, after Python interpreter startup and the
`time` import. `child_startup_ms` is parent invocation to that timestamp, not pure
mount time. `workload_ms` covers open/read/check/close loops; setup imports and
large-range expected-byte preparation are excluded. `child_ready_to_finish_ms`
includes that setup. All payload comparisons must succeed. Application-byte
counts describe the child reads, not libgit2 or physical I/O.

## Verification status

`self-test` tests fixture determinism and exact contents, expanded shared-tree
counts, the child workloads, `cat` output observation, timeout handling,
separate inherited-fd metrics capture, summary exclusion of warmups/failures,
counter schema validation, cache bounds, and rejection of eager-read patterns. It does not mount anything and
does not produce git-on performance evidence.

Local infrastructure checks successfully prepare deterministic fixtures and run
the self-tests. The local development container has no `/dev/fuse`; a skipped
integration suite is not a real-FUSE result. Runtime timing, counter, mutation,
and large-file assertions must be established by successful real-FUSE runs on
the exact tested binary. Retain external stdout/stderr, metrics, JSONL and the
executable hash rather than treating fixture-generation timings or self-tests
as runtime evidence.
