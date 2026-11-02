# Local async-fuser adaptation for Git-on

Upstream: https://github.com/mhambre/async-fuser
Pinned source: `c038f63e39fe2e5f87a5311b222deda4d534c735`.
The version number is inherited from that source; equivalence to a crates.io
release is not asserted. Original MIT copyright/license is in LICENSE.md.

The synchronous protocol implementation and upstream tests are retained.
The asynchronous API uses native local futures without Send/Sync bounds or
async-trait boxing. A single local dispatcher owns a bounded set of outstanding
requests and reusable receive buffers/future slots. It parses each packet once,
continues accepting requests while callbacks wait, and drains owned work before
destroy. AsyncFd observes error readiness as well as readable/writable events,
so an external unmount wakes the transport. `AsyncSession::from_fd` permits the
same wire-level testing as the upstream synchronous session. Async examples use
the changed native-future API. Local regressions are in tests/local_async_dispatch.rs.
The source changes are recorded separately in upstream.patch. Async open/create
responses preserve managed passthrough registrations, and unsupported defaults
retain the original ENOSYS contract. Receive buffers follow the negotiated write
limit; immediately ready requests recycle the same buffer and future slot.

This integration is in progress. The Git-on core must also become asynchronous;
changing only this transport does not make source decoding or spill I/O async.

Session owns its mount and awaits cleanup before runtime shutdown, including
failed INIT. Linux mount IDs prevent cleanup of a replacement mount. Native
mountpoint bytes are preserved. Receive buffers obey FUSE_MIN_READ_BUFFER even
before negotiation. Release builds propagate write errors and short replies.
These fixes have focused regressions with recorded failing predecessors.

The local `shutdown` hook awaits filesystem-owned asynchronous cleanup after
request drain and before destroy, including INIT errors. Local Rc-backed read
and data replies hold immutable payload allocations through reply completion.
Read responses may also retain a local operation owner through nonblocking
reply waits, so filesystem mutation ordering can include the wire completion.
Focused tests keep this owner alive under actual socket backpressure.

Lookup replies expose independent name-binding and attribute TTLs and can retain
a local namespace owner until send/drop. This lets Git-on reuse positive names
while mutable attributes stay live and prevents a namespace mutation from
committing before an older LOOKUP reply finishes.

Attribute replies can retain a local operation owner too. Git-on uses namespace
and inode read ownership for cached local all-policy attributes so namespace
edits and copy-up cannot commit before the old reply finishes. The wire format
and owner-free constructor behavior are unchanged.
