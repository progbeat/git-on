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
2. For a release, replace `RELEASES.md` with `# VERSION`, `<!-- Bump: patch|minor|major -->`,
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
runtime/build dependency closure in `Cargo.lock`, and native package-build/generated release-build jobs, including inherited
workflow `env` and `defaults` settings. Build-job edits are treated conservatively
because a step can change the distributed executable. Test-only dependency updates and edits to other CI jobs
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
and `<!-- Bump: initial -->`. Later PRs cannot reuse this exception.

## CI and automatic publication

The ordinary CI workflow keeps `rust`, `macos`, `real-fuse`, `lazy-performance`,
`release-policy` and real native packaging checks. `package-build` runs stock
`dist build` for Linux and Apple Silicon, exercises extracted binaries and the
stock installer, and checks bundled license texts. Linux runs real FUSE against
the packaged executable; Mac runs CLI/materialized behavior. The required
`package` check succeeds only when both native jobs pass.

After all checks pass on a master push with a validated version increment, the
small `release` job creates `vVERSION` at that exact tested SHA, or checks an
existing tag without moving it. It dispatches the generated cargo-dist workflow
at that tag. Only this trigger needs `contents: write` and `actions: write` on
the built-in `GITHUB_TOKEN`; no additional credential is used. GitHub explicitly
[allows workflow_dispatch from GITHUB_TOKEN](https://docs.github.com/en/actions/concepts/security/github_token),
whereas a tag created with that token would not start a tag-push workflow.
Docs-only changes do not dispatch a release.

`.github/workflows/release.yml` is generated unchanged by pinned cargo-dist.
`dispatch-releases = true` selects its supported dispatch flow; PRs run its
standard plan validation. The stock workflow builds the complete target matrix,
generates installers/checksums/manifests, and handles hosting and publication.
There is no project publisher, installer patcher or second asset-assembly layer.
`RELEASES.md` supplies the current version's notes through standard cargo-dist
changelog discovery; it need not retain release history. The `Bump` directive is
an HTML comment so it does not appear in rendered GitHub release notes.

No tag, dispatch or live release is required to test a PR. Trigger tests use a
fake `gh`; native package/installer tests use local artifacts. Actual publication
is not claimed from these tests. The stock workflow's publication and retry
behavior applies; this project adds no immutability-settings check or custom
post-publication verification. Repository settings remain owner-managed.

## Required repository settings

Repository settings are managed separately. The existing master protection
requires pull requests, up-to-date branches and Actions checks `rust`,
`real-fuse`, `lazy-performance`, `release-policy`, and `package`. The real native
package jobs remain behind that last check. No branch-protection or access
setting is changed by this migration. The automatic release trigger additionally
waits for the Mac CI job. Do not make the publishing workflow a PR-required check.

## Installing a published binary

```sh
curl -fsSL https://sh.progbeat.dev/git-on | sh
curl -fsSL https://sh.progbeat.dev/git-on | GIT_ON_VERSION=1.3.0 GIT_ON_INSTALL_DIR=/absolute/path sh
```

The thin dispatcher selects the latest release installer or the tag requested
with `GIT_ON_VERSION` (optional v prefix). Unset, empty or `latest` selects latest.
It forwards all argv unchanged and leaves installation options, platform
selection and PATH management to the stock installer. Destinations use the
standard `GIT_ON_INSTALL_DIR` environment variable. There is no legacy archive
fallback. The Cloudflare redirect and already published releases are unchanged.

The dispatcher uses ordinary `curl ... | sh -s -- "$@"` semantics: pipeline status
is the shell's, so an upstream HTTP failure can be masked, and some truncated
final invocations can execute. Signalling only the wrapper PID does not guarantee
child cancellation; interrupt the foreground process group normally. These are
the owner's accepted streaming limits, not additional installer guarantees.

## Release artifacts and checksum policy

Cargo-dist generates the Linux x86_64 and Apple Silicon `.tar.gz` archives,
`git-on-installer.sh`, checksum files and its standard manifests/source artifact.
The installed executable is `git-on`; native archives include README, LICENSE
and target-specific THIRD_PARTY_NOTICES. See [platforms](platforms.md) for runtime
requirements and the Mac materialization-only scope.

The installer is unmodified cargo-dist output. It checks archive hashes when
its supported checksum utility is available, but can skip verification when
that utility is missing. In particular, stock macOS `shasum` is not a project-added
fallback. This standard policy is accepted; fail-closed checksum validation is
not promised. HTTPS and the GitHub release source remain the trust boundary.

The bootstrap needs POSIX sh and curl; the stock installer checks its own tools.
Rust, Python and jq are not installation prerequisites. The generated installer's
normal PATH behavior and user environment apply. Private-release authentication
uses its documented environment options; this project creates no credentials.

## Regenerating the workflow

Use the cargo-dist version pinned in `dist-workspace.toml`:

```sh
dist generate --mode=ci
dist generate --mode=ci --check
```

Edit cargo-dist configuration rather than hand-editing generated YAML or installer
scripts. The supported `.github/dist-setup.yml` build hook generates third-party
license texts before packing and preserves the Mac deployment target. The
ordinary CI package checks use the same stock build command and notices helper.

## Building from source

Use Rust 1.89+, native Git, and a C compiler:

```sh
cargo install --path . --locked
```

Mac builds require no FUSE framework or extension: they support startup and
explicit-all materialization. Native mounts remain deferred to issue #12.
Linux uses registry fuser 0.18 with no vendored fork.
