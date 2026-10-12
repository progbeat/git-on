# Remote execution and native verification

`git on REV [OPTIONS] -- COMMAND [ARGS...]` selects a local revision.
`git on REPOSITORY REV [OPTIONS] -- COMMAND [ARGS...]` selects a source and revision.
Put `--keep`, `-k`, and `--verify` after REV and before the first `--`;
the command tail stays literal. URLs work
outside a checkout. A configured remote name requires its local repository.
HTTPS, OpenSSH and scp-style SSH URLs use native Git transport; `file://` also
supports local upload-pack fixtures. URL rewriting is applied once.

Remote revisions accept HEAD, an advertised full ref, an unambiguous branch/tag
name or a full SHA-1 OID. Ancestry, reflog and abbreviated-ID syntax require local
objects and are unsupported remotely. Resolve the ref once, then request only
that OID. Match HEAD and full refs literally; expand only short names to branch
and tag candidates. Unrelated non-UTF-8 ref names do not prevent ASCII selection.
Ref movement cannot change the selected tree. SHA-256 repositories,
submodules and Git LFS remain unsupported.

## Object demand and cache lifetime

Remote execution requires Git 2.45.1+. Fetch the selected object and root tree
before mounting. Each subsequent missing tree or blob is explicitly fetched by
OID with `tree:0`. libgit2 does not automatically fetch promisor objects; native
Git also receives `GIT_NO_LAZY_FETCH=1`. Never request `blob:none`, perform a full
checkout or recursively walk unvisited trees. Subtree selection reads only
ancestor trees. Packs may contain necessary tag/commit ancestry or delta bases;
tests inspect actual received objects, requests and bytes, since the filter's
name alone proves nothing about demand.

The server must advertise filtering before any object transfer. Rejection of a
requested unadvertised OID names the OID and includes Git's server diagnostic.
There is no full-clone fallback. A demanded-object failure returns EIO for the
affected filesystem operation, reports the transport error on stderr and makes
the runner fail even if the command catches EIO and exits successfully.

A pending, unblocked and nonignored SIGINT, SIGTERM, SIGHUP or SIGQUIT in the
task requesting a remote object interrupts that request with EINTR. The runner
polls the requesting task's Linux `/proc` status while its owned Git operation
or cache-lock acquisition waits, because fuser cannot dispatch an INTERRUPT
request while a synchronous callback is blocked. This interruption cancels
only the current demand and preserves the command's signal handler and actual
exit status. It does not latch a transport failure; a handler may return and
retry the read using the same pinned snapshot and owned SSH connection.

Metadata lookup may fetch the requested blob to determine its size: ordinary
upload-pack offers no header-only request. Listing does not fetch child sizes or
payloads. Applicable `.gitignore` blobs are fetched on demand for write policy.

Persistent objects live in `$XDG_CACHE_HOME/git-on/objects-v1`, falling back to
`$HOME/.cache/git-on/objects-v1`. Retain at most 256 MiB and 4096 objects, plus a
zero-length lock. Each record contains its type byte and object bytes. Atomic
insertion and LRU eviction share an exclusive flock. Acquiring the lock reclaims
interrupted staging writes. Check restored object hashes. Each active run copies
demanded cached objects into a private temporary bare repository, so concurrent
eviction cannot invalidate active reads. Oversized objects stay in run storage.

These limits bound persistent retention. Active temporary objects and normal
ignored writes have no runner quota; ordinary system disk limits apply.
Completion removes the temporary remote repository and owned SSH connection.
`--keep` preserves generated files under `backing/` using existing retention and
cleanup rules. Busy mounts preserve recovery state for stale-run cleanup.

## Authentication

Use the caller's effective system/global/repository configuration, expanded
includes, inherited `git -c`, transport and credentials. Replay each value once
for the private repository in its effective order; expand include conditions in
the original caller's context. Do not capture or store passphrases, modify an
agent, or create/change real trust keys. Captured configuration stays in process
memory/environment rather than a runner-owned durable credential file.

Authenticate OpenSSH during ref advertisement before mounting. Use a private
ControlMaster socket. An unlocked agent needs zero key passphrase prompts; an
ordinary selected encrypted identity authenticates once. Lazy fetches reuse the
master, enable BatchMode and forbid replacement TCP connections. Connection loss
fails clearly. Existing OpenSSH host-key policy controls first-contact prompts.

Select the identity using SSH configuration, `GIT_SSH_COMMAND`,
`core.sshCommand`, or a transparent `GIT_SSH` executable forwarding OpenSSH argv,
in that native Git precedence order. Direct SSH commands preserve quoting and
variable expansion. Shell prefixes, compounds, command substitutions and
non-OpenSSH variants are unsupported and fail before authentication. The runner
owns control options, including short `-M`/`-S`; caller `-O` control commands
are rejected. Its master remains available for the run lifetime, and lazy reads
require the existing socket through OpenSSH mux proxy mode. Normal completion
and busy-mount retention close the owned master. Recovery records the socket's
exact device/inode and only closes a matching real socket; changed or symbolic
paths and absent or invalid records are skipped. Uncatchable termination before
initial advertisement finishes can leave an unrecorded connection; stale cleanup
skips it rather than guessing ownership. Ordinary completion and cancellation
close recorded connections.
Multiple encrypted identities, separately authenticated jump hosts and
interactive credential helpers can need extra initial interaction. The one-prompt guarantee covers a directly reachable
host and one selected identity. Lazy fetches disable Git/OpenSSH terminal/askpass
prompts; a helper opening an independent UI is outside that guarantee. Unix
control-socket paths must fit Linux's length limit. Cancellation terminates owned
transport subprocesses and follows the normal unmount/cleanup path.

## Native signature verification

Local execution without `--verify` retains its bundled-libgit2 revision resolver
and does not require or invoke native Git. Remote execution and all `--verify`
runs use native Git as described here.

`--verify` resolves one commit or annotated tag OID, then calls native
`git verify-commit OID` or `git verify-tag OID`. Native Git's exit status decides
success, formats and trust. SSH, OpenPGP and other configured Git formats are not
restricted by git-on. Configure `gpg.ssh.allowedSignersFile` through gitconfig or
inherited `git -c`. No runner trust, fingerprint, signing or key-management
interface exists.

Verify an annotated tag as that exact tag, then peel its recorded targets to a
tree. The target commit need not carry a second signature. A lightweight tag
resolves to its commit, which must verify. Support at most 64 nested annotated
tags. Raw tree/blob IDs fail clearly, with no history search for another object.
Any failed native verification forbids mounting and execution. Git's successful
verification can accept an OpenPGP signature with unknown trust; git-on applies
no additional trust policy. Disable replacements in native Git and read exact ODB
objects in the loader.

For verified `REV:path`, verify REV first and select a relative directory inside
its bound tree. Colon-based searches such as `:/message` are unsupported in this
mode. Preserve existing local revision/path resolution without verification.
Never re-resolve a ref after verification. Success certifies neither passed
tests nor sandbox safety.

## Acceptance evidence

Required CI uses disposable real HTTPS/upload-pack and OpenSSH servers, remote
execution outside a checkout, reference/local-content comparison, exact OIDs,
tags, subtrees, named remotes and moving refs. Cold/warm tests inspect requests,
requested OIDs, bytes and received objects, including a million distinct-file
digit tree. SSH tests count encrypted-identity/agent prompts and authenticated
connections, refuse authentication and drop the owned connection. Cache tests
cover concurrent eviction, byte/entry bounds and interrupted writes. Native SSH
and OpenPGP fixtures cover verification, effective config, tags, raw trees,
replacements and ref races. Real mounts additionally cover remote ignored writes,
keep/cleanup/cancellation and transport errors caught by the child. Loader tests
without FUSE are distinct evidence. Public GitHub smoke is optional external
evidence, separate from deterministic required gates. Consult exact-head CI
results before treating any planned scenario as verified.
