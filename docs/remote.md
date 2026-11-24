# Remote execution and native verification

`git on REV [OPTIONS] -- COMMAND [ARGS...]` resolves one local revision through
native Git. `git on REPOSITORY REFSPEC [OPTIONS] -- COMMAND [ARGS...]` uses the
reusable protocol v2 path described below or passes both operands unchanged
to native `git fetch` and selects the fetched object. Git-on parses its own
options and command separator. Native Git supplies repository resolution and
handles refspecs outside the reusable path.
Runner options may appear anywhere before the first `--`, interspersed with
the two operands; the command tail stays literal. URLs work
outside a checkout. A configured remote name requires its local repository.
Configured remote names, ordinary local paths, `file://`, HTTPS, OpenSSH and
scp-style SSH URLs use native Git repository resolution and transport. Effective
URL rewriting is applied once. Authentication and server capabilities still
determine whether execution can proceed.

Native fetches apply inherited Git configuration within the
[service-state contract](specification.md#service-state). Git-on disables
hooks, including `reference-transaction` for updates in a private repository.
Snapshot contents come directly from Git objects. Native Git never needs a
worktree checkout to prepare them.

Fetch syntax such as `heads/main`, `tags/release`, `+main` and
`main:refs/heads/demo` is interpreted by Git. A destination ref is written only
in the private temporary repository, never the caller's repository. In two-operand
mode, `main:directory` is a fetch refspec, not a snapshot-subdirectory selector.
Use the command's arguments or working-directory logic to choose a directory
inside that fetched snapshot. One-operand local `HEAD:directory` remains native
Git tree selection.

Git decides which object-ID abbreviations and revision expressions are valid:
local revision resolution and fetching do not necessarily accept the same forms.
A full commit ID selects a fixed revision even if an upstream branch later
moves. With native fetch, Git-on uses the single distinct object selected by
`FETCH_HEAD`. Subsequent reads request exact OIDs. A fetch
that selects multiple distinct objects cannot define one snapshot and fails
before mounting.
SHA-256 object storage, submodules and Git LFS remain unsupported.

## Object demand and cache lifetime

Remote execution requires Git 2.45.1+. For lazy execution, fetch the selected
object and root tree before mounting. Each subsequent missing tree or blob is
explicitly fetched by OID with `tree:0`. libgit2 does not automatically fetch promisor objects; native
Git also receives `GIT_NO_LAZY_FETCH=1`. Never request `blob:none`, perform a full
checkout or recursively walk unvisited trees for readonly operations. Subtree
selection reads only ancestor trees. Writes in `all` prepare private copies of
the affected files. Packs may contain necessary tag/commit ancestry or delta bases;
tests inspect actual received objects, requests and bytes, since the filter's
name alone proves nothing about demand.

Where protocol v2 and the caller's policy permit, exact demands reuse
invocation-owned channels. Native Git resolves the original repository and
prepares the OpenSSH arguments. Local paths and `file://` URLs can stream a
full OID, an unambiguous branch, or `HEAD` from `upload-pack` from the first
filtered fetch. Git's `fetch-pack --diag-url` resolves paths that need URL
parsing. Its private Git directory is also available to the compatibility
path. The stream does not write a pack or index there. Named selections
on file sources advertising `ref-in-want` use native fetch, as do file sources
with unknown formats. For every transport, native Git applies caller settings
outside the streaming path when it selects the initial revision.
A full SHA-1 object ID or unambiguous branch can use the reusable HTTPS channel
for the initial fetch after the server advertises filtering. Lazy HTTPS resolves
a named branch with `ls-refs` and requests the resulting OID, including when the
server advertises `ref-in-want`. With a full OID, the first fetch can handle
the POST authentication challenge directly. A non-pack reply to the first
fetch can require a ref query to finish authentication before retrying the
same OID. If HTTPS falls back to native Git before decoding the first pack,
native fetch keeps any OID already selected by the reusable channel. A named
remote with Git's usual branch-to-tracking map uses the same streamed selection
as its effective URL. With default fetch mappings, an unambiguous source-only
force refspec such as `+main` uses the same stream as `main`. Explicit
destinations, custom fetch mappings and remote groups use native Git.
An SSH identity that
authenticates without a prompt can select the branch or exact object ID and
fetch the root tree over one owned channel. Other refspecs use native Git's
selection with the caller's credentials, proxies and TLS policy.
Interactive preparation uses native fetch with job-control supervision. Git-on
owns the terminal and presents standard Git/OpenSSH askpass requests. Helpers
request interaction through `GIT_ASKPASS` or `SSH_ASKPASS`. Configured askpass
programs retain their selection policy and original environment. Once the
command starts, HTTP(S) helpers run in their own sessions without a controlling
terminal.
SSH reuses only the existing identity-checked owned master. Explicit protocol v0/v1,
custom server/vcs/server-option settings, explicit fsck policy, or a custom
`GIT_EXEC_PATH` retain the native compatibility path. Git's standard inherited
helper path supports reuse.

On FUSE and NFS, protected directory and file lookups can wait for an exact
object independently of the filesystem worker. A cold pathname lookup can
wait for its parent tree, then resolve the name and wait for its file size.
Directory enumeration can wait for its tree as well. On eligible HTTPS mounts,
an asynchronous loader multiplexes exact requests on one HTTP/2 connection.
For a supported host-scoped HTTP Basic challenge, native Git reads existing
credentials with `git credential fill` in the caller's context. Helpers can
provide a username and password or continue an exchange with pre-encoded
Authorization values as the server changes challenges. Both streaming and
native HTTPS dispatch only credential lookups. Stored credentials and cookie
files retain their contents after either outcome. Other initial challenges
and credential policies use native Git's HTTP exchange. Libcurl and the peer
govern the active streams. The loader can briefly wait for
overlapping demands after confirming a scan or observing consistently slow
replies.
Each foreground fetch can group up to 64 already pending OIDs.
Explicit Git HTTP configuration, authentication outside the supported exchange,
and servers without HTTP/2 or filtered protocol v2 use the native helper.
Transport selection completes before any object demand is sent.

When the native ref advertisement supplies capabilities, Git-on captures
the packet trace in a private temporary file and scans bounded chunks for
filter support. Detached writers can finish using their open descriptors.
Later opens of the trace path go to the null device during the run.
The runner can emit the captured advertisement to the caller's stdout or
stderr. Paths to external trace files and pipes remain untouched.

Native helpers start with four available channels. If each of the first eight
successful transfers takes at least 64 milliseconds, the pool can use up to
16 channels. It can expand earlier if a scan confirms its forecasts while the
connection is still being measured. SSH opens its channels through the same
owned master and uses fewer when the server refuses an additional channel.
On a slow SSH startup, up to two spare channels can begin negotiating protocol
v2 as soon as the authenticated, owned socket exists. When the caller leaves
the protocol version unconfigured, an unambiguous named branch can use a Git
v0 ref advertisement while filtered protocol v2 channels negotiate on the
same master. A ready channel
can fetch the selected commit while another channel awaits an `ls-refs` reply.
Object requests on this path use filtered protocol v2.
If fetching the selected object also takes at least 64 milliseconds, up to two
more SSH channels can start negotiating while the root tree is fetched.
Ready startup channels can serve exact demands while other channels are still
negotiating and the pool is measuring its first replies.
An established SSH channel can serve exact demands when the opening limit
shrinks. When ordinary slots are unavailable, a released slot can reconnect
for an exact demand while another pack or startup negotiation is in progress.
If the reopen attempt is refused during startup, the slot retries after a
standby finishes negotiating. Recovery through a slot that has never opened
waits for active packs and startup attempts to finish.
When another channel opens during execution, pending requests stay eligible
on the existing channels until one is available to send them.
Beyond the initial SSH pool, actual demands open additional channels.
Prefetch can use those channels after they connect.
Before the first enumeration of a snapshot directory, a slow pathname lookup
or file read can send SSH EOF with the final request on a channel while
another negotiated channel stays ready for the next read. Enumeration keeps
channels reusable for the reads it can trigger. Pack verification and the
channel's exit status still determine whether each request completed
successfully.
An idle pool starts an ordinary request immediately. While a transfer is active,
the queue can wait up to two milliseconds on a slow source or during a confirmed
scan. For a confirmed scan, the wait can reach twelve milliseconds while the
source is slow or still being measured. On FUSE over HTTP/2, a required tree
bypasses this wait so a pathname operation can discover its next dependency.

A first access to a regular file in a directory opened for enumeration finds
uncached and previously unscheduled blob OIDs in Git's depth-first order
through already opened directories. It skips previously considered ranges
and examines at most 512 remaining entries. The same planner serves FUSE and
NFS. Speculative packs start with up to four OIDs. Confirmed first accesses to
predicted objects can expand this to 64, 128 or 256 at any connection speed.
The first confirmation can expand the window to 64 after a slow reply when two
or more required objects are still pending. Consecutive misses reduce the
window. On a fast source, batches follow the file being read in depth-first
order. While measuring a connection or on a slow source, a dense scan can
group remaining OIDs from the current and recently opened directories.
The predictor tracks at most 512 OIDs ahead of actual accesses.
When the window fills after read-ahead has expanded or on a slower connection,
completed predictions can age out after unrelated accesses. Ordinary demands
take precedence and can join an active speculative request without duplicating
its fetch. If that speculation fails, waiting ordinary demands can retry.
A later read of an unused prediction needs another fetch or confirmation
from a complete alternate pack.
A diagnosed pack failure after serving a required read fails the invocation.
After a successful command exit, the runner waits for validation of the packs
that supplied its reads. A separate large-object decode permit lets that
validation proceed when unrelated streams occupy the ordinary permits. The
runner sends EOF to SSH channels once their final requests are submitted,
including while receiving a pack the command already used. It checks the
exit status of channels that supplied reads and are idle once pack validation
finishes. It cancels unrelated requests still in flight.
An exact transfer can fail independently of the invocation when validated
extras from another pack complete every demand and supply the requested data.
These paths require a reusable transport. The specification's
[resource assumptions](specification.md#resource-assumptions) apply to remote
execution.

The HTTPS and SSH receivers share the same incremental Git framing and pack
decoder. It inflates ordinary entries in memory and verifies the canonical Git
OID against the requested OIDs before notifying a waiting read. It can publish
a verified object while the rest of its pack is still arriving. The receiver
also verifies the complete pack's checksum and framing. The receiver
applies deltas to verified bases in memory from the first fetch, including
when preparing the selected revision. If a delta precedes its base, the receiver
keeps its decoded instructions until the base arrives.
Multiplexed HTTPS decodes response bodies off the connection driver, with
per-stream backpressure. Requested blobs enter the authenticated, run-local store.
Delta reconstruction borrows verified bases from the active pack. Extra
objects retain those buffers for later access and become readable after the
pack is verified. A demand for a known entry can wait for validation without
another fetch. Eager materialization writes a verified extra directly into
its destination.
The native Git compatibility path can use temporary pack files.

Explicit-all eager materialization needs the entire selected tree. Fetch the
selected revision once with depth one and without an object filter. Reusable
HTTPS, SSH, local paths and `file://` URLs can use a full OID, an unambiguous
branch, or the advertised `HEAD`. The file transport uses the
installed Git's `upload-pack` under its default transport policy. HTTPS
accepts HTTP/2, HTTP/1.1, and HTTP/1.0 responses for this one-time transfer.
For SSH and `file://`, close the request stream after sending the fetch and
verify the process exit status after receiving the complete pack. Decode the pack in RAM
and write verified file contents to their
destination paths. If automatic mount setup fails and copying is allowed, the
same transfer can use the already selected OID. Native Git handles competing
refs, named fetches on servers advertising `ref-in-want`, other refspecs,
custom fetch mappings, and signature verification. Explicit
Git settings outside those handled by the streaming path go through native
fetch, including settings that Git validates before accepting a pack.
In that path, retain the compressed pack and decode
directly into destination files. Large blobs in the same staged pack share a
single native batch decoder. Both paths reuse already prepared objects.
Both paths ask for a shallow snapshot and can operate without server-side
filtering. Git receives the server's original challenge values during
credential lookup. A rejected credential ends the exchange. Native Git handles
repository- or user-scoped credentials and other authentication challenges.

A temporary Git repository serves native fetch and signature verification.
Git's file-URL diagnostic can also need a Git repository to parse escaped
local URLs. The pack reader opens the indexes Git already built from a
separate object directory. For local revisions with nested alternates or
byte paths, a read-only object-location bridge represents the source layout
for libgit2. Index expressions such as `:path` use a private Git context
that directs split-index timestamp updates to service storage. Native Git
reads the primary index, its referenced base and objects from the source.
An alternate index outside the Git directory may need a private copy of its
referenced base to preserve Git's lookup order. Standard streamed HTTPS,
SSH and plain local paths need no temporary Git repository, including during
credential lookup. This includes `origin` with the standard tracking refspec
when its resolved URL streams.

Native object directories, verification scratch and any regular files used
for diagnostics are memory-backed. See
[temporary storage](usage.md#temporary-storage) for the platform settings.

When materialization fetches missing objects by OID, at most 256 known object
IDs from the current directory can share one native `fetch --stdin` and
decoder operation. Each object is checked for existing run-local availability
and deduplicated before requesting it. These requests use `tree:0`.
All paths request only known objects. The batch
size bounds request memory/argv work, not repository or file counts. Metrics
separately report requested-object count (`remote_fetches`) and actual native
fetch/decode operations; full-process tracing checks those operation counts.
Session request/decode counts and transport process starts are separate from
native fetch/cat-file counters. Server-side Git subprocesses remain real work,
included in waited CPU for local source fixtures.

For filtered lazy execution, the server must advertise filtering before any
object transfer. Rejection of a requested unadvertised OID names the OID and
includes Git's server diagnostic.

Two-operand local-path and `file://` sources use upload-pack and require the same
capabilities. Ordinary local repositories do not enable them by default. When
you control the source repository, configure it before fetching through git-on:

```sh
git -C ../another-repo config uploadpack.allowFilter true
git -C ../another-repo config uploadpack.allowAnySHA1InWant true
git on ../another-repo main -- ls
```

These settings belong to the source repository. One-operand local revision
resolution reads its Git objects directly and needs no upload-pack configuration.

There is no full-clone fallback. A demanded-object failure returns EIO for the
affected filesystem operation, reports the transport error on stderr and makes
the runner fail even if the command catches EIO and exits successfully.
This includes `.gitignore` object acquisition during file/directory
creation. A healthy policy denying a write instead returns EROFS.

A source that is temporarily slow is awaited: delay alone does not make Git-on
return an I/O error or invent EINTR. Git-on adds no read deadline. Actual Git,
transport, integrity and storage errors still fail the relevant operation;
caller-configured transport timeouts remain effective.

Git-on does not emulate FUSE_INTERRUPT by reading the requesting process's
`/proc` signal state. The current fuser backend does not implement that protocol
operation. A signal sent only to a command blocked in FUSE may therefore be
processed after the outstanding I/O completes, including Ctrl+C handled by the
command. If the source never responds, such signal handling can remain delayed.
To cancel the whole invocation independently of that I/O, signal Git-on itself.
Runner-directed termination and observed command exit stop owned operations
before unmount/cleanup; they do not depend on requester polling. Native
request-level interruption remains a TODO for a backend that supports it.

Metadata lookup may fetch the requested blob to determine its size: ordinary
upload-pack offers no header-only request. Listing does not fetch child sizes or
payloads. Applicable `.gitignore` blobs are fetched on demand in default
`ignored` mode. `none`, `untracked` and `all` do not need ignore-rule blobs for
write decisions. The same [four policies](usage.md#filesystem-behavior) apply to
local and remote snapshots.

Each remote invocation owns an invocation-local object store. The direct
reusable HTTPS and SSH startup paths keep the selected commit or tag and its
root tree in memory. Native fetch, signature verification and native interactive
preparation can use a private temporary bare repository.
The streamed HTTPS credential lookup uses the caller's configuration and
working directory. Reusable fetches keep verified Git objects in memory.
Demanded objects remain available until cleanup,
including after decoded memory-cache eviction.
There is no global persistent object cache or cross-run reuse. A later invocation
resolves its operand through native Git again and fetches its own demanded trees
and blobs. Git-on does not read, modify, migrate or delete historical
`$XDG_CACHE_HOME/git-on/objects-v1`, `objects-v2`, or corresponding
`$HOME/.cache/git-on` directories. HOME/XDG_CACHE_HOME are not required for an
object cache; native Git's normal configuration and authentication environment
still applies. No daemon or background cache service is started.

Prepared blobs remain reusable across close/reopen and eviction. Native decoder
stdout feeds OID verification into a private RAM buffer. A failed consumer
stops and reaps the decoder, and an unsuccessful native exit fails the import.
Local loose objects feed the same store in chunks. Libgit2 still decodes
packed/delta objects as required.

[Resources and integrity](resources.md) describes the whole-blob and block LRUs,
disk-block authentication, and private writable copies. Raw disk files are
opened relative to the owned directory, with type, owner, link count and length
checks. The store retains one last-read descriptor independent of object count
or open child handles. Compressed staging, retained raw files and active reader
buffers are accounted separately and released with the invocation.

Libgit2's pack descriptor cache retains up to 32 descriptors. Operating system
errors propagate before launch or through the lazy operation and runner
failure as applicable. No failure reruns a started child or silently changes
write policy.

Active temporary objects and normal ignored writes have no runner disk quota;
ordinary system disk limits apply. The block cache budget includes entry
overhead. Completion removes any temporary remote repository and owned
SSH connection. Write required results to explicit external paths. Busy mounts
preserve recovery state for stale-run cleanup. Explicit-all materialization
eagerly fetches every selected tree entry; it never bypasses authentication,
verification or object errors. See [resource acceptance](resources.md).

SIGKILL cannot run cleanup handlers. The runner does not request FUSE
`auto_unmount`: after its FUSE descriptor closes, the mount remains disconnected
and fresh directory access fails with `ENOTCONN`. The command's separate process
group and an established persistent SSH master can survive. The workspace
keeps its memory-backed recovery data while a recorded SSH master or a
retained run needs it. Kernel locks are released when their owning descriptors
close. Stop the command and other mount users yourself,
then run `git on --cleanup <temporary-directory>` to unmount and release recorded
resources. Recovery never kills processes using stale PID records. A kill during
mount setup can leave the same disconnected mount even before the command starts.

## Authentication

Use the caller's effective system/global/repository configuration, expanded
includes, inherited `git -c`, transport and credentials. Replay each value once
for the private repository in its effective order; expand include conditions in
the original caller's context. Do not log or persist passphrases, modify an
agent, or create/change real trust keys. Captured configuration stays in process
memory/environment rather than a runner-owned durable credential file.

Authenticate OpenSSH during ref advertisement before mounting. Use a private
ControlMaster socket. An unlocked agent needs zero key passphrase prompts; an
ordinary selected encrypted identity authenticates once. Lazy fetches reuse the
master, enable BatchMode and forbid replacement TCP connections. Connection loss
fails clearly. OpenSSH uses the caller's existing trusted host keys.
Unknown hosts require trust to be established before running Git-on.
The runner disables host-key updates and forwarding.
During native Git authentication, Ctrl-Z stops the invoking job and returns
control to its shell. `fg` resumes preparation with the terminal; `bg` leaves the shell
foregrounded, so another authentication read can stop the job until `fg`.

Select the identity using SSH configuration, `GIT_SSH_COMMAND`,
`core.sshCommand`, or a transparent `GIT_SSH` executable forwarding OpenSSH argv,
in that native Git precedence order. Direct SSH commands contain an executable
and options, preserving quoting and variable expansion. Git supplies the host
and remote command; caller positional operands are rejected before authentication.
Shell prefixes, compounds, command substitutions and
non-OpenSSH variants are unsupported and fail before authentication. The runner
owns control options, including short `-M`/`-S`; caller `-O` control commands
are rejected. Expanding the owned socket path in caller options cannot bypass
control-option sanitization. Its master remains available for the run lifetime,
and lazy reads use that socket. Extra SSH channels ask the master to open a
session. When the master refuses, its existing channels stay available and a
replacement network connection is blocked. Normal completion and busy-mount
retention close the owned master. Recovery records the socket's
exact device/inode and only closes a matching real socket; changed or symbolic
paths and absent or invalid records are skipped. Uncatchable termination before
initial advertisement finishes can leave an unrecorded connection; stale cleanup
skips it rather than guessing ownership. Ordinary completion and cancellation
close recorded connections.
Shutdown validates the selected client's canonical path, file identity and
SHA-256 content fingerprint, including for clients outside `PATH`. Selected
clients must be readable regular executable files. The private record stores
those identities, not executable contents, SSH arguments, configuration or
credentials. Equal-length edits with restored timestamps still fail validation.
A changed or unavailable client causes a visible cleanup failure and retains
the socket and run directory. After validation, shutdown sends the OpenSSH
mux version 4 termination request directly over the owned control socket; it
never executes the selected client or wrapper again. This removes the pathname
check/use race without relying on a mutable executable fd or changing wrapper
interpretation. The exchange has a single one-second deadline and bounded
packets; unsupported greetings, refusals and stalls retain recovery state.
Restore that client, or close the identified socket with a trusted OpenSSH client,
then retry checked cleanup. An already dead owned endpoint can be removed after
its socket identity is checked again. Older metadata-only client records lack a
content fingerprint: close their live master with a trusted client before
retrying cleanup. Legacy records without any selected-client entry can still
close their identity-checked master through the same direct mux protocol.
OpenSSH checks existing host keys at the destination and at each configured
jump. Every hop uses the run's read-only host-key policy.
With `CheckHostIP=yes`, OpenSSH checks existing address entries as usual.
Git-on directs additions to `/dev/null` and keeps the configured files in
the lookup order. A host-file setting that cannot be determined unambiguously
stops SSH setup.
OpenSSH discovers its default user and system configuration files in their
original contexts. A launcher in service memory passes the read-only options
to implicit jump clients and to `ProxyCommand ssh -W …` with literal
OpenSSH options, including when an argv-forwarding wrapper changes the
OpenSSH executable name. Literal `nc` network relays also work, including
OpenSSH's documented `nc -X connect -x proxy %h %p` form. Proxy commands
that require an arbitrary shell program are unsupported. An explicit
`-F` profile keeps its user
configuration context. When setting `GIT_ON_SERVICE_TMPDIR` for a proxied
connection, choose a path OpenSSH can pass to its generated shell command
without quoting.
Apple's OpenSSH combines reading and storing passphrases in its
`UseKeychain` option. Git-on keeps that option disabled for new connections
to preserve the caller's Keychain. OpenSSH can use identities already
available through the caller's agent or ask for a passphrase at startup.
Multiple encrypted identities, separately authenticated jump hosts and
interactive credential helpers can need extra initial interaction. The one-prompt guarantee covers a directly reachable
host and one selected identity. Object loading during preparation can request
credentials through Git-on. After command startup, lazy fetches disable
Git/OpenSSH terminal/askpass prompts; a helper opening an independent graphical
UI is outside that guarantee. Unix
control-socket paths must fit Linux's length limit. Cancellation terminates owned
transport subprocesses and follows the normal unmount/cleanup path.

## Native signature verification

All runs use native Git to interpret positional operands. Local execution resolves
the unchanged revision in the caller's repository, then the object loader reads
that pinned object through bundled libgit2. Remote execution resolves the selected
object using the [transport rules above](#object-demand-and-cache-lifetime).
`--verify` adds native Git's signature check to either mode.

`--verify` resolves one commit or annotated tag OID, then calls native
`git verify-commit OID` or `git verify-tag OID`. Native Git's exit status decides
success, formats and trust. SSH, OpenPGP and other configured Git formats are not
restricted by git-on. Configure `gpg.ssh.allowedSignersFile` through gitconfig or
inherited `git -c`. No runner trust, fingerprint, signing or key-management
interface exists.

Git-on gives native Git a private directory for signature scratch files.
When Git invokes a GnuPG verifier, Git-on copies its public keyrings and trust
inputs into service storage. GnuPG selects its configuration, including
version-specific options, from the private home. Configured keyring and trust
paths resolve to copies there. Configured verifier programs can also read
additional verification files at their original paths relative to that home.
X.509 verification can use the caller's cached CRLs from a private copy.
When a configured wrapper invokes GnuPG by name, Git-on also prepares the
home the wrapper selects through `GNUPGHOME` or `--homedir`.
Configured wrappers and other verifier programs must keep their own writes
within the service directory supplied for verification.
The verification agent uses private sockets.
Verification diagnostics use the inherited diagnostic stream.

Verify an annotated tag as that exact tag, then peel its recorded targets to a
tree. The target commit need not carry a second signature. A lightweight tag
resolves to its commit, which must verify. Annotated tag chains are peeled to
their selected tree without imposing an arbitrary runner depth limit. Raw
tree/blob IDs fail clearly, with no history search for another object.
Any failed native verification forbids mounting and execution. Acceptance is
exactly native Git's result under the caller's effective configuration; git-on
adds no signer allowlist or trust threshold. Git's default OpenPGP verification
can accept a valid signature with unknown trust. Configure Git's
`gpg.minTrustLevel` (for example, `fully`) when the calling workflow requires a
stricter threshold: if native Git rejects the object, git-on rejects it before
mounting or execution. The calling workflow chooses and validates the configured
policy it uses as execution approval. Disable replacements in native Git and read
exact ODB objects in the loader.

Native Git resolves the complete local operand before verification. A local
`REV:path` therefore selects a raw tree or blob; those objects have no signature
and `--verify` refuses them. To verify the containing commit, select `REV` with
`--verify` and change directory inside the command instead. Git's revision-search
syntax remains Git's responsibility; git-on does not replace it with a ref-name
whitelist. Remote refspec destinations do not select subtrees.
Never re-resolve a ref after verification. Success certifies neither passed
tests nor sandbox safety.

## Acceptance evidence

Required CI uses disposable real HTTPS/upload-pack and OpenSSH servers, remote
execution outside a checkout, reference/local-content comparison, exact OIDs,
tags, subtrees, named remotes and moving refs. Independent-run and same-run reuse tests inspect requests,
requested OIDs, bytes and received objects, including a million distinct-file
digit tree. SSH tests count encrypted-identity/agent prompts and authenticated
connections, refuse authentication and drop the owned connection. Object-storage
tests verify same-run availability, fresh-run fetches, no dependence on global
cache paths and preservation of historical cache directories. Native SSH
and OpenPGP fixtures cover verification, effective config, tags, raw trees,
replacements and ref races. Real mounts additionally cover remote ignored writes,
keep/cleanup/cancellation and transport errors caught by the child. Loader tests
without FUSE are distinct evidence. Public GitHub smoke is optional external
evidence, separate from deterministic required gates. Consult exact-head CI
results before treating any planned scenario as verified.
