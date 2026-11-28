# Remote execution and native verification

`git on REPOSITORY REV -- COMMAND` accepts a configured remote name, a local
path or `file://` URL, or a network URL. Effective Git URL rewrites apply once.
A named remote uses the effective `remote.<name>.url` setting.
Repository-local settings apply when Git discovers that repository.
SCP-style addresses such as `git@github.com:OWNER/REPO.git` select SSH.
To select a local path such as `name:repo`, use `./name:repo` or an absolute path.
A local source always uses native Git revision syntax, including `HEAD:path`,
and honors the same index and object environment as `git -C PATH on REV`.
A local partial clone requires a Git version that supports `--no-lazy-fetch`
and can use the objects already available to that repository.

## Network profile

GitHub.com is the compatibility reference. Other domains can be used when
their servers implement the same profile.

| Transport | Protocol |
| --- | --- |
| Direct HTTPS | TLS, HTTP/2 and Git protocol v2 |
| Direct OpenSSH | SSH with Git protocol v2 forwarded to `git-upload-pack` |

Git 2.45.1 or newer is required for remote execution. Network sources must
provide SHA-1 object IDs and support exact-object wants and filtering. Specify
`HEAD`, an exact advertised ref such as `refs/tags/v1.2.3`, an unambiguous
short branch or tag, or an available full SHA-1 object ID. Remote ref names
use UTF-8. An object ID also works when a ref name uses another encoding.
Choose the full ref to disambiguate names. A remote's fetch refspec maps
references for native Git fetches and does not determine which tree Git-on
selects.

HTTP/1.1, HTTP proxies, Git v0 and v1, alternate SSH clients, custom upload-pack
programs and fetch destinations are outside this profile. Git-on uses direct
HTTPS connections even when HTTP proxy environment variables are present. Use
the system trust configuration, or provide a CA bundle using `http.sslCAInfo`
or `GIT_SSL_CAINFO`. Within a worktree, relative paths in transport settings
resolve from its root. Explicit native Git HTTP settings that require a
different transport are rejected.

HTTPS follows initial discovery redirects on the same scheme, host and port
unless `http.followRedirects=false` is set. For path-scoped credentials the
supported GitHub flow resolves the redirect before credential lookup.
`http.userAgent` configures the user-agent header. Git protocol response
bodies use identity content encoding. HTTPS reference selection accepts
replies up to 256 KiB. Select by full SHA-1 OID when a reference reply is larger.

## Authentication and state

HTTPS sends the single effective URL-matched `http.extraHeader` when configured.
For URLs without user information, libcurl can read matching `.netrc` entries.
For HTTPS credential lookup, the supported GitHub profile advertises Basic
in its own `WWW-Authenticate` field. When discovery, ref listing, or an object
request receives this challenge, Git-on passes the effective repository URL
and any supplied credentials to `git credential fill` through its standard
input for read-only lookup.
After launching the command, credential lookup runs without interactive prompts.
Git-on does not ask credential helpers to store or erase credentials. SSH uses
OpenSSH's configured identity and agent and requires a previously trusted host
key. Host-key files and the agent's identity list stay unchanged.
Lazy mounts reuse an invocation-owned OpenSSH control connection and close it
during teardown. If that connection is lost, outstanding reads fail.
A direct SSH copy uses a single foreground SSH process. Copy after a mount
fallback can reuse the connection already established for that mount.

Git-on connects directly even when OpenSSH configuration contains
`ProxyCommand` or `ProxyJump`. The direct profile accepts `-J none` in a
configured SSH command. Specifying a jump host with `-J` returns an error.
OpenSSH can run explicitly configured `Match exec` commands when evaluating
connection settings, including while opening additional channels. Those
commands have their configured effects. The [source contract](specification.md#source-selection-and-observable-state)
applies to Git-on and its ordinary helper operations.

## Object loading

Native mounts request the commit or tag and the root tree before launch.
They fetch further trees and blobs by exact OID as the command reaches them.
Directory listings do not fetch the contents of child files. A lookup can
fetch a blob for its size because upload-pack has no header-only request.
The planner starts with four candidates from already opened directories. It
can batch overlapping requests and grow read-ahead when first accesses use
the forecast. A request for an object already in flight joins that request.
If a filtered multi-object response omits a want, only the missing OID is
retried on its own.

The mount can read a verified wanted object before its pack ends. A delta
base received as an extra becomes available to other requests after the
pack and transport complete. The completed response can satisfy an exact
request already in flight, independently of the duplicate transfer's outcome.
After the user command exits normally, Git-on checks the outcome of
responses that served it. A response that waits for more pack data
for 30 seconds during this check fails the run, even if keepalive packets
arrive. Other in-flight requests can be cancelled during teardown.
A temporary cache placement failure is reported for the affected object.
It can be retried after capacity returns. The rest of its response still
undergoes normal validation.

Git-on decodes and verifies objects directly from the stream. Cached objects
share verified buffers. Blobs up to 64 KiB share a 16 MiB LRU with checked
blocks from prior spills. A separate LRU holds the eight most recently used
complete large blobs. An evicted blob can spill to invocation-local
storage. Block digests and loaded tree metadata remain in memory. Later disk
reads check the block digests before returning bytes.

The copy backend requests one shallow pack for the chosen revision and
writes its visible tree before launch. Copy requires the complete verified
pack and successful transport completion before starting the command.
A 128 MiB payload LRU holds complete
verified blobs until materialization consumes them. The inflating object
and Git metadata use additional memory. Copy and mounting stream pack data
without a private fetch repository, service refs, pack files or indexes.
Submodules, Git LFS payloads and SHA-256 object stores are outside the
supported profile.

## Native signature verification

`--verify` invokes native `git verify-commit` with the selected commit OID,
or `git verify-tag` with the selected annotated tag OID. With a lightweight
tag, Git-on verifies the object the tag selects. The native verifier uses the
effective Git configuration and its ordinary keyring, trust database and
profile. The native Git verification invocation and its configured verifier
retain their ordinary effects. Git-on creates a private Git
context only for remote verification, to make the exact signed object
available to native Git. It removes that context on completion and failure.
Locally Git reads the object from the chosen source repository.

The invocation uses the same verified object even if the ref moves during
verification. For an annotated tag, its target is taken from the verified
tag object. If Git rejects the signature or the selected object becomes
unavailable, the user command does not run.
