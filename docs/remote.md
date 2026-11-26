# Remote execution and native verification

`git on REV [OPTIONS] -- COMMAND [ARGS...]` resolves one local revision through
Git. `git on REPOSITORY REVISION [OPTIONS] -- COMMAND [ARGS...]` loads one
snapshot from the effective repository URL or local path. Options can appear
anywhere before the first `--`. The command after it receives the remaining
arguments verbatim. A configured remote name such as `origin` requires the
caller's repository. Git resolves the name and applies `url.*.insteadOf` once.

## Network transport profile

GitHub.com is the reference for network compatibility. Hosts on other domains
work when they offer the same Git behavior for these operations.

| Transport | Required protocol |
| --- | --- |
| `https://host/owner/repo.git` | Direct TLS with HTTP/2 and smart Git protocol v2 |
| `ssh://git@host/owner/repo.git` and `git@host:owner/repo.git` | Direct OpenSSH with Git protocol v2 |

Both transports use SHA-1 object IDs and shallow fetches by exact OID. Lazy
mounts additionally require `filter tree:0`. The server must supply the objects
reachable from the selected OID when later requests name them explicitly.
Git-on selects a ref once and sends subsequent object requests by OID. Network
execution requires Git 2.45.1 or newer for configuration, credential lookup and
signature checks.

Select `HEAD`, a full SHA-1 OID, a full ref such as `refs/heads/main`,
`refs/tags/v1.0`, or `refs/pull/41/head`, or an unambiguous short branch or tag.
The branch and tag shorthand forms `heads/main` and `tags/v1.0` are also
supported. Network ref arguments use UTF-8. If names compete, use a full ref.
The optional leading `+` and an explicit
`:refs/...` suffix select the source. Network loads create no destination
ref, and a configured remote's tracking map has no effect on this selection.
Wildcard refspecs, remote groups and abbreviated OIDs are outside this profile.
For network revisions, a colon separates the refspec source and destination.
Choose the working directory from the command to run inside a network subtree.

HTTP/1.x, Git protocol v0/v1, plain HTTP, `git://`, external Git remote helpers
and proxies are unsupported. Use the canonical repository URL for HTTPS.
Git-on treats redirects as errors. HTTPS ignores proxy environment variables and
rejects explicit Git HTTP overrides. It uses normal TLS certificate validation,
with `GIT_SSL_CAINFO` available for a custom CA bundle. SSH accepts direct ports
and host aliases, including a direct connection to `ssh.github.com:443`, and
rejects `ProxyCommand` and `ProxyJump`. Network loads reject explicit Git
transport-policy overrides, custom upload-pack or server options, fetch fsck
settings and custom `GIT_EXEC_PATH`. Unsupported network requests exit with
a diagnostic.

## Explicit local repositories

`git on /path/to/repository REV -- COMMAND`, a `file://` URL, and a configured
remote name that resolves to a local repository use the same revision and
object reader as `git -C /path/to/repository on REV -- COMMAND`. Relative
paths are resolved from the invocation directory. An existing directory uses
ordinary Git discovery, including the repository of an enclosing worktree.
An existing directory such as `work:tree` takes precedence over OpenSSH's
colon shorthand. Use an explicit `ssh://` URL to disambiguate a network source.
Prefix a relative path with `./` if its name has the form `helper::address`.
Git-on can try an omitted `.git` suffix when Git finds no repository from the
given path. The suffixed path must identify a repository of its own. Local
paths accept Git's `~/` expansion and gitfiles. Git interprets `REV` as a
local revision, including `HEAD:subdirectory`.

Git-on reads the original repository directly. Local loading needs no
upload-pack settings, temporary fetch repository, or copy of the source's
objects. If maintenance removes a still-unread object, the operation asking
for it reports a source read error.

Git-on fixes the selected OID before execution. Later changes to a ref leave
the selection unchanged. SHA-256 object storage, submodules and Git LFS
remain unsupported.

## Object demand and cache lifetime

Remote execution requires Git 2.45.1+. For lazy execution, fetch the selected
object and root tree before mounting. Each subsequent missing tree or blob is
explicitly fetched by OID with `tree:0`. Local object access through native
Git uses `--no-lazy-fetch`. Missing objects in a local partial clone produce a
source error. Never request `blob:none`, perform a full
checkout or recursively walk unvisited trees for readonly operations. Subtree
selection reads only ancestor trees. Writes in `all` prepare private copies of
the affected files. Packs may contain necessary tag/commit ancestry or delta bases;
tests inspect actual received objects, requests and bytes, since the filter's
name alone proves nothing about demand.

SSH selects a ref and fetches the initial object on one owned channel.
Later channels reuse the same OpenSSH master. When an authenticated channel
waits for the Git advertisement, a spare can prepare on that master.
A slow startup prepares spare channels while the first object arrives. The
pool opens more channels when reads need them and continues using established
channels when the server refuses a session. When execution requires a mount
and two spares are ready, Git-on sends EOF with the startup root-tree
request. It can also send EOF with an initial slow lookup and prepare a
replacement while a ready channel serves subsequent requests. The pack and
SSH exit status confirm completion.

HTTPS uses one HTTP/2 connection per session. On a Basic authentication
challenge, Git-on uses `git credential fill` in the caller's context, then uses
the returned username and password for requests to that repository. Libcurl
can try `.netrc` before the helper lookup, as native Git does. A rejected
helper result ends the exchange. Git-on sends no approval or rejection to
credential helpers and keeps stored credentials and cookies intact. Libcurl and the server govern
HTTP/2 concurrency and flow control.

On FUSE and NFS, protected directory and file lookups can wait for an exact
object independently of the filesystem worker. A cold pathname lookup can
wait for its parent tree, then resolve the name and wait for its file size.
Directory enumeration can wait for its tree as well. Each foreground fetch
can group up to 64 already pending OIDs. Both transports group tree and blob
requests into separate packs. If the server omits a requested descendant
from a valid filtered pack, Git-on retries that OID on its own.

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
Explicit-all eager materialization needs the entire selected tree. It
requests one shallow snapshot pack by OID without an object filter. Git-on
decodes the pack in RAM and writes verified contents to their destination
paths. Over SSH, Git-on closes the request stream after sending the fetch and
checks the process's exit status after receiving the complete pack. If automatic mount setup fails and copying is allowed, the copy keeps
the already selected OID. The network transfer works without server-side
filtering in eager mode.

### Temporary Git contexts

- For a streamed snapshot, Git-on exposes only the selected commit or tag
  as a loose object in its temporary service directory for native verification.
  Git uses the caller's repository context when available. Outside a repository,
  it uses minimal bare repository metadata and the configuration already
  evaluated in the caller's context.
  A private config directory carries a SHA-256 caller's repository-local
  verifier settings into the SHA-1 context. Local verification uses the caller's
  object database.
- Index expressions such as `:path` use a private context for split-index
  timestamp updates. Git reads the index and its objects from the source.
  An alternate index may require a private copy of its referenced base to
  preserve Git's lookup order.

Network loading and direct local reads take no temporary Git repository for
object transfer. Local reads use libgit2 where it can open the selected
store directly. Reftable repositories and object layouts such as nested or
byte-valued alternate paths use one `git cat-file` process in the source
repository context. Credential lookup can use a private directory for its
temporary files. See [temporary storage](usage.md#temporary-storage) for
filesystem selection.

Metrics count requested objects separately from streamed packs and
transport starts. Count the work of fixture servers separately when
comparing client operations.

For filtered lazy execution, the server must advertise filtering before any
object transfer. Rejection of a requested unadvertised OID names the OID
and includes Git's server diagnostic.

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
root tree in memory. HTTPS credential lookup uses the caller's
configuration and working directory. Reusable fetches keep verified Git objects in memory.
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

Completion removes the run's private files and closes the owned SSH connection.
After abrupt termination, the workspace retains private recovery data
while the recorded SSH master or a busy run needs it. See
[foreground lifecycle and cleanup](usage.md#foreground-lifecycle-and-cleanup)
and [resource limits](resources.md).

## Authentication

Use the caller's effective Git configuration for URL resolution and credential
lookup, and OpenSSH's effective configuration for direct SSH. Native signature
verification honors expanded includes and inherited `git -c`. Replay each value once
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
During interactive authentication, Ctrl-Z stops the invoking job and returns
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
OpenSSH checks existing host keys at the destination, including address
entries with `CheckHostIP=yes`. Git-on preserves the configured lookup order
and directs any additions to a private target. It uses `/dev/null` when
there is room in the lookup list and a private copy of the first file at
OpenSSH's list limit. An ambiguous host-file setting stops SSH setup.
OpenSSH reads default and explicitly selected configuration files in their
original contexts.

OpenSSH evaluates `Match exec` while it reads configuration, including for
`ssh -G`. It can run those commands again during connection setup. The effects
of these explicitly configured commands are covered by the
[service-state exception](specification.md#service-state).

Apple's OpenSSH combines reading and storing passphrases in `UseKeychain`.
Git-on disables it for new connections to preserve the caller's Keychain.
OpenSSH can use an unlocked key from the caller's agent or prompt once for a
selected encrypted key. During preparation, the terminal integration presents
configured Git and OpenSSH askpass requests. After the command starts, Git
and OpenSSH terminal and askpass prompts are disabled. A configured helper may
open independent graphical UI. Unix control-socket paths must fit the host's
length limit. Cancellation terminates owned transport subprocesses and follows
the normal unmount and cleanup path.

## Native signature verification

Local execution uses native Git to resolve
the unchanged revision in the caller's repository, then the object loader reads
that pinned object through bundled libgit2. Remote execution resolves the selected
object using the [transport rules above](#object-demand-and-cache-lifetime).
`--verify` adds native Git's signature check to either mode. A streamed copy
keeps the snapshot in memory through verification and materializes it from
the same transfer. For a streamed tag, Git-on compares the target in the
payload verified by Git with the target in the received object. It checks the
temporary object against the selected OID before using the snapshot.

`--verify` resolves one commit or annotated tag OID, then calls native
`git verify-commit OID` or `git verify-tag OID`. Native Git's exit status decides
success, formats and trust. SSH, OpenPGP and other configured Git formats are not
restricted by git-on. Configure `gpg.ssh.allowedSignersFile` through gitconfig or
inherited `git -c`. No runner trust, fingerprint, signing or key-management
interface exists.

Native signature verification uses the caller's verifier environment,
including its home, temporary paths, keyrings, trust data, and agent setup.
The [service-state exception](specification.md#service-state) permits the
ordinary effects of a direct `git verify-commit` or `git verify-tag`, including
writes directed by the configured verifier. Diagnostics use the inherited
streams. Cleanup covers Git-on's own temporary data.

Verify an annotated tag by the OID of the tag object. Its signature covers the
recorded target OID. Follow those recorded targets to the selected tree even
if the tag ref moves after verification. Selecting a commit OID or a
lightweight tag requires the commit's signature. Annotated tag chains are
peeled to their selected tree. An unavailable object needed to identify the
verified tree prevents launch. Raw tree and blob IDs fail clearly.
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
and OpenPGP fixtures cover verification, caller configuration and effects,
tags, raw trees, and replacements. A ref-movement test moves an annotated tag
after native Git has accepted its exact tag OID and checks the original tree.
Real mounts additionally cover remote ignored writes,
keep/cleanup/cancellation and transport errors caught by the child. Loader tests
without FUSE are distinct evidence. Public GitHub smoke is optional external
evidence, separate from deterministic required gates. Consult exact-head CI
results before treating any planned scenario as verified.
