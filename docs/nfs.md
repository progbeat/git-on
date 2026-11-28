# macOS NFSv4.1 profile

On macOS 26 and later, git-on exports one pinned Git tree and an invocation-local
writable layer to the built-in Darwin client. The Rust server uses the shared
Overlay for Git object verification, inode identity, copy-on-write and all four
write policies. The server opens a Unix stream socket in a short 0700 directory
under `/private/tmp`. The writable backing stays in the [private run directory](usage.md#standard-streams-and-temporary-writes).
Only the mounting user and the kernel can access the local endpoint. The process uses
`mount_nfs` with NFSv4.1 and `AUTH_SYS`; it needs neither a TCP listener nor
rpcbind. Symlink targets and component names are handled as bytes.

The server provides client and session establishment, fore-channel binding and
sequence-slot replay. It implements regular file opens, share reservations,
byte-range locks, directory lookup and enumeration, hard links, symlinks,
namespace mutations, supported Unix metadata, and stable writes and commits.
The export has volatile session state and a lifetime limited to the invocation.
File handles identify the export and an inode. A handle from a completed run
returns `STALE`. An open file can be read or modified through its open state
after its last link is removed. Delegations, pNFS, callbacks, named attributes,
ACLs and Kerberos are outside this profile. Darwin can request AppleDouble
sidecar files for platform metadata. Those requests follow the selected write
policy. The Darwin client can read byte-valued paths and symlink targets. On
macOS 27.2, a first direct shebang launch through a symlink to a name containing
invalid UTF-8 can hang in the native executable loader. Passing the script to
its interpreter explicitly works. This local implementation has no independent
protocol or security audit.

## Limits and object access

RPC records are limited to 2 MiB, individual read and write payloads to 1 MiB,
and each COMPOUND to 64 operations. The server limits active connections,
queued requests and session slots. Expired client state releases its open
handles, locks and directory views. Each session slot caches its last reply
including a successful prefix followed by an error. Mounts with parallel remote
object reads can negotiate up to 32 slots. The negotiated maximum reply size
keeps each session's replay storage within 16 MiB. Git data requests can remain
outstanding until they complete or the invocation is cancelled.
Short GETATTR and LOOKUP compounds can use an independent slot while another
slot waits for a Git object. A single client session can send these calls under
different AUTH_SYS credentials. Permission checks use each call's credentials.

The mount can start without traversing descendant trees or reading their files.
GETATTR obtains the requested attributes. Directory listing uses the shared
Overlay cursors. A Git blob whose size is unknown can be listed with its other
attributes, with size resolved on a subsequent named attribute request.
WRITE honors the requested stability and COMMIT synchronizes the private
backing file, including after the writing open has closed. A backing write
or sync failure also fails the invocation so it remains visible when Darwin
consumes an error while flushing buffered writes.

## Cleanup

Git-on normally unmounts while the listener and worker still serve the client.
It then stops the listener, connections and worker before deleting the temporary data. If cleanup fails, it reports the retained run
directory. Stop processes using the mount, then run
`git on --cleanup <temporary-directory>`. Cleanup checks recovered run identity
against the system's NFS mount entries and refuses to delete a directory that
still contains a mount. If the embedded server has exited, recovery detaches the
identified stale mount without waiting for the dead endpoint to flush.

## Verification

The macOS CI job runs real mounts as an ordinary user and checks the filesystem
and write-policy fixtures shared with FUSE, including lazy reads, byte-valued
paths, executable modes, shared mmap, locks and fsync. It also requires expected results from a pinned
selection of upstream pynfs tests using the production Unix listener. These
tests exercise the supported profile with two independent clients.

The `nfs_rpc` fuzz target exercises record framing and the shared XDR/RPC
decoders. From the repository root, run `cargo +nightly fuzz run nfs_rpc`.
The [benchmark harness](../tests/benchmark.md) compares NFS with materialization
on the same macOS host and Git tree.
