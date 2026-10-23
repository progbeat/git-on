# Releases and installation

Changes to the released CLI, its runtime dependencies or binary build settings
require one binary release. Documentation, GIFs, tests, standalone examples and
`install.sh` alone do not change the binary version or publish a release.
Squash, ordinary merge and rebase merging are supported. Squash merging is
recommended: the resulting commit contains both a binary change and its release
metadata. No separate release PR, manual tag, or CHANGELOG history is required.

## Preparing a change

1. Determine whether the change affects the binary. For a release, update
   `Cargo.toml` and the root `git-on` entry in `Cargo.lock` to exactly one
   increment above **current master**, not merely the branch's original base.
2. For a release, replace `.github/release-notes.md` with `# VERSION`, `Bump: patch|minor|major`,
   then concrete user-facing Markdown notes. Mention changed behavior, fixes,
   compatibility implications and migration steps. This is the next release's
   body, not an ever-growing changelog; previous notes live in GitHub Releases.
3. Run `python3 tests/test_release_policy.py`, the normal checks, and
   `python3 scripts/release_policy.py --base origin/master` after fetching master.
4. When another PR merges first, update from master and recalculate any required version.
   Never reuse a released version. Multiple commits within one PR share one bump.

Nonrelease changes retain current master's package version and release notes;
an unnecessary version bump is rejected. Mixed binary/documentation changes
share one release. The policy compares `src/`, explicit binary/library and build
script paths, Rust toolchain and `.cargo/` settings, Cargo binary settings, the
runtime/build dependency closure in `Cargo.lock`, and the distribution `package`
job, including inherited workflow `env` and `defaults` settings. Package-job
edits are treated conservatively because a step can change the distributed
executable. Test-only dependency updates and edits to other CI jobs
do not require a binary release unless they also change binary inputs.
When a crate has separate runtime/build and dev versions, classification matches
the declared runtime/build version to its locked edge. If the manifest and lock
cannot identify one edge, validation fails explicitly instead of guessing its kind.

Use patch for backwards-compatible binary bug fixes. Use minor for new
backwards-compatible capabilities, such as an optional write policy. Use major for
breaking the documented CLI, filesystem, environment or platform contract.
Correcting git-on's custom argument handling to restore the intended native Git
semantics is a bug fix; a release that also adds write policies uses a minor bump.
This classification includes using native Git for local resolution: direct
`git-on` invocations now require Git on `PATH`, including unverified local runs
that version 1.1 could resolve without it. Maintaining a separate Git-free
resolver would contradict the original requirement that Git itself interpret
the operands. This restoration is explicitly classified as a bug fix, rather
than a breaking platform change; unrelated platform requirements still follow
the major-release rule above.
These rules also apply before 1.0: this project deliberately reserves major bumps
for breaking changes instead of silently treating every 0.x release as breaking.
CI can validate the numeric increment and notes; semantic classification requires
review of the change. Agents are responsible for proposing the correct kind.

Bootstrap is the only exception: a base with no Cargo package introduces `0.1.0`
and `Bump: initial`. Later PRs cannot reuse this exception.

## CI and automatic publication

`release-policy` determines whether a binary release is required. For a release
it rejects unchanged, decreasing, skipped, malformed or mismatched versions and
missing/placeholder notes. All PRs reject stale branches and package/lockfile
version mismatches. Nonrelease PRs must retain the current master version.
All required checks, including `package`, run on every PR regardless of this
decision; no path filter hides a required check.
The shared native cargo-dist packaging and immutable draft-publication sequence
are specified below. The existing release decision and exact-master-commit
requirements still apply. All native build, artifact integrity, installed binary,
Linux real-FUSE and performance gates must succeed before publication.

## Required repository settings

CI code alone cannot prevent a merge. Configure protected master (or an equivalent
active ruleset) to require pull requests, strict up-to-date branches and these
checks from GitHub Actions: `rust`, `real-fuse`, `lazy-performance`,
`release-policy`, `package`. Disallow force pushes/deletion and bypasses. Do not
make `release` a PR-required check: it deliberately runs only after merging.
Enable squash merging; do not merge multiple independent release changes in one
master push. A failed release can be retried; no workflow silently republishes
or moves an existing version.

On 2026-10-12, the public repository's active `master-protection` ruleset requires
pull requests, squash merges, resolved review threads and the five strict status
checks listed above, and forbids deletion and force pushes. Repository settings
are managed separately from this workflow. Strict protection closes the race
where master advances after the PR's version check finishes.

## Installing a published binary

The entry point remains unchanged:

```sh
curl -fsSL https://sh.progbeat.dev/git-on | sh
curl -fsSL https://sh.progbeat.dev/git-on | sh -s -- --version 1.3.0 --bin-dir /absolute/path
```

`install.sh` selects the latest generated release installer or the tag requested
with `--version` (optional v prefix). It does not validate semantic versions or
ranges: a missing tag/installer asset fails through HTTP, without executing the
partial download. No legacy installer path exists. The Cloudflare redirect and
published releases are unchanged.

The dispatcher downloads the complete release installer into a private temporary
file, checks curl's status, then runs it. It parses only version selection and
`--bin-dir`, translated to `GIT_ON_INSTALL_DIR` because cargo-dist has no such CLI
option. All remaining arguments are passed unchanged to the generated installer,
which owns `--help`, `--quiet`, `--verbose` and unknown-option handling. No shell
evaluation, version/API/digest lookup, duplicate destination validation or jq
branch is present.

The generated installer chooses the platform, verifies its embedded archive
SHA256 and installs the binary. `dist-workspace.toml` sets the default destination
and disables updater packaging. The bootstrap does not override PATH-management
or updater policy; cargo-dist's generated defaults and user environment apply.
Running again updates or reinstalls. Earlier releases without a generated
installer asset fail rather than selecting an old archive format.

### Release format and trust boundary

Pinned cargo-dist 0.33.0 builds:

- `git-on-x86_64-unknown-linux-gnu.tar.gz` (Ubuntu 22.04 / glibc 2.35+).
- `git-on-aarch64-apple-darwin.tar.gz` (native Apple Silicon, macOS 13+).
- `git-on-installer.sh`, per-asset `.sha256` checksums, `source.tar.gz`,
  `release.json` provenance and a complete `SHA256SUMS` inventory.

The installed binary is always `git-on`; archive root directories match their
archive names without `.tar.gz`. README, MIT LICENSE and target-specific THIRD_PARTY_NOTICES accompany the executable.
Intel macOS is not implied by the arm64 artifact. See [platforms](platforms.md)
for runtime prerequisites and the Mac materialization-only scope.

Stock cargo-dist can skip hashing when `sha256sum` is absent.
`scripts/harden_dist_installer.py` is the explicit reviewed generation-time
correction: require valid embedded SHA-256 for both archives, use `sha256sum` or
macOS `shasum -a 256`, and fail on missing/malformed/mismatched hashes, failed hash
tools or absence of both tools. Hash verification precedes extraction/install,
so those failures leave the previous executable unchanged. The correction also
cleans the private download directory on ordinary failures and handled signals.
Template drift fails generation. CI tests the actual corrected output and both
packaged native binaries, including corruption and repeat installation.

Archive hashes detect mismatch/corruption within the GitHub release trust
boundary. They neither authenticate the bootstrap independently nor protect
against a compromised release source. The thin bootstrap needs POSIX sh, curl,
awk and ordinary temporary-file tools; Rust, Python and jq are not dependencies.

### Inspect first or use an authenticated private release

Download and inspect `install.sh` before running if desired. For private releases,
use an existing authenticated gh account to download and inspect the release
installer and archives manually,
then use its documented `GIT_ON_GITHUB_TOKEN` support if required. The dispatcher
never creates credentials or reads a token. No external authentication is configured.

### Shared packaging and publication

`dist-workspace.toml` pins cargo-dist 0.33.0; `scripts/fetch_dist.sh` pins the tool
archive hashes. The custom `ci.yml` integrates its native local/global build
phases with the existing release-policy gate rather than a separate tag workflow.
The local matrix builds both native archives, verifies their checksums and probes
the extracted executable. Global generation receives both verified archives;
the correction embeds their SHA-256, then assembly records the exact asset set.
`verify_dist.py` checks integrity again across assembly, install and publication
jobs. Both hosts install/update the actual archive through the actual corrected
installer; Linux additionally runs non-skipped FUSE acceptance. macOS CLI and
materialized tests validate its supported 1.3.0 subset; Mac mounts are deferred.

Only a validated master push can execute publication. The publisher requires
release immutability already enabled (read-only setting check), creates/resumes a
draft, uploads every archive/installer/metadata/checksum asset, downloads and
verifies the complete exact set while still draft, then publishes. It verifies
GitHub reports `immutable: true` afterward. No assets are appended after
publication; existing mismatching releases are never repaired. Repository settings
must be managed separately; this PR is not permission to publish or change them.

### Building from source

Use Rust 1.89+, native Git, and a C compiler:

```sh
cargo install --path . --locked
```

macOS builds require no FUSE framework or extension: 1.3.0 supports startup
and explicit-all materialization only. Native Mac mount support is deferred to
issue #12. Linux builds use registry fuser0.18 with no vendored fork.
