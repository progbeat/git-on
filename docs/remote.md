# Remote execution and native verification

`git on REV [OPTIONS] -- COMMAND [ARGS...]` resolves one local revision through
native Git. `git on REPOSITORY REFSPEC [OPTIONS] -- COMMAND [ARGS...]` passes its
two positional operands unchanged to native `git fetch`, then selects the fetched
object. git-on parses its own options and command separator; Git parses the
repository and revision/refspec. No runner URL, ref-name or object-ID syntax
check is applied to those operands.
Runner options may appear anywhere before the first `--`, interspersed with
the two operands; the command tail stays literal. URLs work
outside a checkout. A configured remote name requires its local repository.
Configured remote names, ordinary local paths, `file://`, HTTPS, OpenSSH and
scp-style SSH URLs use native Git repository resolution and transport. Effective
URL rewriting is applied once. Authentication and server capabilities still
determine whether execution can proceed.

Native fetches honor inherited Git configuration, including configured hooks.
For example, a destination ref update can invoke `reference-transaction` through
the caller's `core.hooksPath`. Native Git writes those destination refs in the
private repository; hooks remain ordinary user programs with access to host
paths outside the snapshot. Reading the snapshot performs no checkout and runs
no checkout filters or worktree hooks.

Fetch syntax such as `heads/main`, `tags/release`, `+main` and
`main:refs/heads/demo` is interpreted by Git. A destination ref is written only
in the private temporary repository, never the caller's repository. In two-operand
mode, `main:directory` is a fetch refspec, not a snapshot-subdirectory selector.
Use the command's arguments or working-directory logic to choose a directory
inside that fetched snapshot. One-operand local `HEAD:directory` remains native
Git tree selection.

Git decides which object-ID abbreviations and revision expressions are valid:
local revision resolution and fetching do not necessarily accept the same forms.
A full commit ID pins the source even if an upstream branch later moves. git-on
uses the single distinct object selected by the fetch's `FETCH_HEAD`, and
subsequent reads request exact OIDs rather than resolving the ref again. A fetch
that selects multiple distinct objects cannot define one snapshot and fails
before mounting.
SHA-256 object storage, submodules and Git LFS remain unsupported.

## Object demand and cache lifetime

Remote execution requires Git 2.45.1+. Fetch the selected object and root tree
before mounting. Each subsequent missing tree or blob is explicitly fetched by
OID with `tree:0`. libgit2 does not automatically fetch promisor objects; native
Git also receives `GIT_NO_LAZY_FETCH=1`. Never request `blob:none`, perform a full
checkout or recursively walk unvisited trees. Subtree selection reads only
ancestor trees. Packs may contain necessary tag/commit ancestry or delta bases;
tests inspect actual received objects, requests and bytes, since the filter's
name alone proves nothing about demand.

Explicit-all eager materialization already needs every entry it prepares. For
that backend, at most 256 known object IDs from the current directory share one
native `fetch --stdin` and decoder operation. Each object is checked for existing
run-local availability and deduplicated before requesting it; all use `tree:0`.
This does not add prefetch to lazy FUSE or fetch unknown descendants. The batch
size bounds request memory/argv work, not repository or file counts. Metrics
separately report requested-object count (`remote_fetches`) and actual native
fetch/decode operations; full-process tracing checks those operation counts.

The server must advertise filtering before any object transfer. Rejection of a
requested unadvertised OID names the OID and includes Git's server diagnostic.

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

A pending, unblocked and nonignored SIGINT, SIGTERM, SIGHUP or SIGQUIT in the
task requesting a remote object interrupts that request with EINTR. The runner
polls the requesting task's Linux `/proc` status while its owned Git operation
waits, because fuser cannot dispatch an INTERRUPT
request while a synchronous callback is blocked. This interruption cancels
only the current demand and preserves the command's signal handler and actual
exit status. It does not latch a transport failure; a handler may return and
retry the read using the same pinned snapshot and owned SSH connection.

Metadata lookup may fetch the requested blob to determine its size: ordinary
upload-pack offers no header-only request. Listing does not fetch child sizes or
payloads. Applicable `.gitignore` blobs are fetched on demand in default
`ignored` mode. `none`, `untracked` and `all` do not need ignore-rule blobs for
write decisions. The same [four policies](usage.md#filesystem-behavior) apply to
local and remote snapshots.

Each remote invocation owns a private temporary bare repository. Demanded
objects remain there until that run is cleaned up, so repeated access does not
fetch an available object again, including after decoded memory-cache eviction.
There is no global persistent object cache or cross-run reuse. A later invocation
resolves its operand through native Git again and fetches its own demanded trees
and blobs. Git-on does not read, modify, migrate or delete historical
`$XDG_CACHE_HOME/git-on/objects-v1`, `objects-v2`, or corresponding
`$HOME/.cache/git-on` directories. HOME/XDG_CACHE_HOME are not required for an
object cache; native Git's normal configuration and authentication environment
still applies. No daemon or background cache service is started.

Newly fetched packs are decoded by native Git directly into raw run storage and
discarded before the next demand. Blob payloads are not recompressed into loose
copies; commit/tree/tag metadata remains in the native verification ODB. Pack
discovery does not enumerate earlier per-object packs. Large non-delta blobs
stream during import; Git still performs required delta decoding. The snapshot
reuses its Repository and available objects do not trigger unconditional ODB
refreshes. Canonical OID verification completes before raw bytes become readable.
Local loose objects stream in 64 KiB chunks with cancellation checks.

All prepared blobs survive close/reopen and bounded-memory-cache eviction in
private temporary storage until invocation completion. A disk-resident
randomized hash index avoids one retained descriptor or RAM entry per blob.
Payload, authentication tags and index are logical files split into at most
1 GiB parts, further capped by inherited `RLIMIT_FSIZE`. Each retains one open
part, plus one shared directory descriptor: four retained descriptors, five
during geometric index growth, independent of bytes/object/part counts. Parts
are created/reopened relative to the owned directory descriptor with no symlink
following; their directory stays outside the execution view. They are removed
when their owning readers/store are dropped and remain under invocation cleanup.
Native output passes through a bounded pipe into these parts, with no additional
payload disk copy or per-object header/decode prepass. A per-invocation memory-only HMAC key authenticates
the index and 4 KiB logical payload blocks; range reads verify the returned bytes
without decoding or hashing the entire blob again. This detects modification of
prepared disk bytes under the interview's process-memory protection assumption.
Readers hold weak references to cached memory and use authenticated raw bytes
after eviction. Retained-raw disk metrics are separate from active-reader and
bounded payload-memory metrics. No cross-run reuse is introduced. libgit2's local
pack backend still requires object/delta decoding, and its loose reader maps the
compressed source file. Neither mapped OS page cache nor necessary packed decode
is reported as a retained application payload cache.

Retained libgit2 pack descriptors are capped before threads start: at most 32,
and at most a quarter of the effective soft `RLIMIT_NOFILE` (minimum one pack
window). This leaves descriptors for native Git startup, the mount and active
files; it does not raise OS limits or reserve a quota for the child. Active
operations can still encounter ordinary OS limits, reported before launch or
through the lazy operation and runner failure as applicable. No failure reruns
a started child or silently changes write policy.

Active temporary objects and normal ignored writes have no runner disk quota;
ordinary system disk limits apply. Decoded memory caches retain their byte and
entry budgets. Completion removes the temporary remote repository and owned
SSH connection. Write required results to explicit external paths. Busy mounts
preserve recovery state for stale-run cleanup. Explicit-all materialization
eagerly fetches every selected tree entry; it never bypasses authentication,
verification or object errors. See [resource acceptance](resources-1.4.md).

SIGKILL cannot run cleanup handlers. The runner does not request FUSE
`auto_unmount`: after its FUSE descriptor closes, the mount remains disconnected
and fresh directory access fails with `ENOTCONN`. The command's separate process
group and an established persistent SSH master can survive. Run directories,
private objects and exact Git pins remain; kernel file locks are released when
their owning descriptors close. Stop the command and other mount users yourself,
then run `git on --cleanup <temporary-directory>` to unmount and release recorded
resources. Recovery never kills processes using stale PID records. A kill during
mount setup can leave the same disconnected mount even before the command starts.

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
During native Git authentication, Ctrl-Z stops the invoking job and returns
control to its shell. `fg` resumes Git with the terminal; `bg` leaves the shell
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
and lazy reads require the existing socket through OpenSSH mux proxy mode. Normal completion
and busy-mount retention close the owned master. Recovery records the socket's
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
the socket, pin and run directory. After validation, shutdown sends the OpenSSH
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
Multiple encrypted identities, separately authenticated jump hosts and
interactive credential helpers can need extra initial interaction. The one-prompt guarantee covers a directly reachable
host and one selected identity. Lazy fetches disable Git/OpenSSH terminal/askpass
prompts; a helper opening an independent UI is outside that guarantee. Unix
control-socket paths must fit Linux's length limit. Cancellation terminates owned
transport subprocesses and follows the normal unmount/cleanup path.

## Native signature verification

All runs use native Git to interpret positional operands. Local execution resolves
the unchanged revision in the caller's repository, then the object loader reads
that pinned object through bundled libgit2. Remote execution fetches the unchanged
repository/refspec pair and pins its selected object. `--verify` adds native Git's
signature check to either mode.

`--verify` resolves one commit or annotated tag OID, then calls native
`git verify-commit OID` or `git verify-tag OID`. Native Git's exit status decides
success, formats and trust. SSH, OpenPGP and other configured Git formats are not
restricted by git-on. Configure `gpg.ssh.allowedSignersFile` through gitconfig or
inherited `git -c`. No runner trust, fingerprint, signing or key-management
interface exists.

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
