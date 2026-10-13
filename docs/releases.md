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
`package` builds on Ubuntu 22.04 using Rust 1.90.0, packages Linux x86_64 and tests
the **extracted archive executable** against a real FUSE mount. The GNU binary's
supported baseline is glibc 2.35, not arbitrary Linux. The build vendors libgit2;
FUSE mounting still requires a usable `/dev/fuse` and `fusermount3` on the host.
Release builds use optimization level 3, fat link-time optimization and one
codegen unit to reduce the executable size. This increases release build/link
work; debug builds and runtime dependencies are unchanged.

After a master push, the same workflow waits for `rust`, `real-fuse`,
`lazy-performance`, `release-policy` and `package`. If the validated policy says
no binary release is needed, `release` is skipped and existing versions are not
republished. Otherwise, only then its `release` job
creates `vVERSION` at that exact tested commit, uploads the archive, SHA256SUMS
and source/build metadata to a draft GitHub Release, then publishes it. It never
checks out moving master to choose release contents. Release notes come from
that commit. The token has read access in test/build jobs and contents-write only
in the publishing job; no personal token, crates.io account or app is required.

Master validation compares against the push event's `before` SHA, so a rebased
PR may contain a version bump followed by additional commits. Force pushes and
branch-creation events are rejected by this release validation.

Tag creation is an atomic GitHub ref creation. A tag at another SHA is an error,
never force-moved. Re-running the same commit resumes a draft with matching notes
and assets. An incomplete `starter` upload can be removed and retried only while the
verified release is still a draft; published assets are never repaired by deletion.
Matching published releases are verified and left unchanged. Different
commits can finish out of order without canceling each other's publication.
GitHub may require additional workflow permissions when publishing an older
commit after a newer commit changed workflow files. In that case publication
fails visibly; this pipeline does not request a broader token or promise every
historical release can be published with `contents: write` alone.
The tag-created event is not used to trigger another workflow, because GitHub's
GITHUB_TOKEN normally does not trigger such workflows.

If publication fails, rerun the failed workflow for that exact master commit.
A failure before upload completion leaves a draft, not a partially published
release. Checksums detect corruption; they are not a separate cryptographic
identity/signature. Reproducible archive metadata does not claim independently
reproducible compilation across changing toolchains/runners.

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

The installer needs Python 3.8+ and supports **Linux x86_64 with glibc 2.35+**
(Ubuntu 22.04 baseline), plus the shared zlib (`libz.so.1`) and GCC
(`libgcc_s.so.1`) runtimes confirmed by the packaging job. Its executable probe
catches missing runtime libraries before replacing an existing installation.
It installs a prebuilt binary; Rust, a C compiler, jq,
and sudo are not needed. The same command installs or updates to GitHub's latest
stable release each time it runs:

```sh
curl -fsSL https://raw.githubusercontent.com/progbeat/git-on/master/install.sh | sh
```

Stable binaries are published in [GitHub Releases](https://github.com/progbeat/git-on/releases).
Anonymous downloads require a public repository. For private releases or API
rate limits, use your existing GitHub CLI authentication as described below.
A failed release download leaves the current executable in place.

To pin a version or choose another user-owned installation directory, first
download the script as shown in the next section, then pass the desired option:

```sh
sh install.sh --version 0.1.0
sh install.sh --bin-dir "$HOME/bin"
```

Both `0.1.0` and `v0.1.0` are accepted; prerelease versions are not. Omitting
`--version` selects the latest stable release, even when git-on is already
installed. An explicit older version intentionally downgrades. Nothing runs in
the background, and the installer never edits shell profiles or installs system
packages. The default destination is `$HOME/.local/bin/git-on`. Add that directory
to `PATH` if prompted. Existing symlink/non-regular destinations, symlink directory
components and directories with group-write or other-write mode bits are
refused. These bits describe permissions, not proof that another user has
effective access. Diagnostics show the directory's path, mode and owner.
For a user-owned personal directory with unnecessary group writing, consider
removing just that permission; otherwise select a separate `--bin-dir`.

### Inspect first or use an authenticated private release

Piping a script to a shell executes code from the repository. To inspect it first,
download it, review it, then run the saved copy:

```sh
curl -fsSL https://raw.githubusercontent.com/progbeat/git-on/master/install.sh -o install.sh
less install.sh
sh install.sh
```

For a private repository, use an **already authenticated** GitHub CLI account with
repository access:

```sh
gh api --hostname github.com 'repos/progbeat/git-on/contents/install.sh?ref=master' \
  -H 'Accept: application/vnd.github.raw+json' > install.sh
# Review the saved script before running it.
sh install.sh --github-cli
```

An authenticated source checkout can also run `sh install.sh --github-cli`.
`--github-cli` uses existing gh authentication for metadata and release assets;
it creates no credentials and puts no token into a URL, script, or log. It also
works for authenticated public downloads when the anonymous GitHub API is rate
limited. The normal mode does not read GitHub token environment variables.

### Verification and update safety

The installer resolves one release, checks its exact asset names, verifies the
archive and build metadata against `SHA256SUMS`, and rejects unexpected archive
members and links. New archives include the project's MIT `LICENSE`; the original
v0.1.0 layout remains accepted. It bounds compressed and expanded input sizes, reads only the
expected executable, and checks its reported version before an atomic replacement.
A failed download, checksum, archive check, version probe, or replacement leaves
the previous executable in place. Rerunning the same version is safe. Temporary
files are cleaned on ordinary failures and handled interruption signals; an
uncatchable SIGKILL or power loss can leave temporary files.

Checksums detect corruption; they are not independent signatures. The script,
archive, and checksum file share the GitHub repository's trust boundary. To pin
the installer code as well as the binary version, inspect a copy from the matching
release tag or an explicitly selected commit instead of the moving master URL.

The bootstrap uses an HTTPS URL with curl's default redirect and TLS settings.
The installer's release downloader separately rejects non-HTTPS redirects.
The script encloses its payload in a function and guards the final call, so an
incomplete download cannot start installation or drop supplied arguments. See the installer regressions in
the [test guide](../tests/README.md#installer-acceptance).

Install the distribution's `fuse3` package separately when needed. A working
`fusermount3`, usable `/dev/fuse`, and permission to mount FUSE are required to run
git-on. Missing FUSE setup produces an installation warning; it does not prevent
installing a verified binary. Containers must expose the device and allow mounts.
Do not run the installer as root to bypass unavailable FUSE setup.

### Building from source

From a checkout of the revision you want to install, with Rust 1.89+ and a C
compiler:

```sh
cargo install --path . --locked
```

Ensure Cargo's binary directory, normally `~/.cargo/bin`, is on `PATH`.
The Linux/FUSE runtime requirements still apply.

## Design references

- [GitHub strict required checks](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-protected-branches/about-protected-branches)
- [GitHub token-trigger behavior](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/trigger-a-workflow)
- [Cargo compatibility guidance](https://doc.rust-lang.org/cargo/reference/semver.html)
- [release-plz release PR flow](https://release-plz.dev/docs/usage/release-pr)
- [cargo-dist configuration](https://axodotdev.github.io/cargo-dist/book/reference/config.html)

release-plz's separate release-PR workflow and cargo-dist's broader distribution
machinery are useful for other policies. This project's single supported target
and release-on-binary-change policy use a smaller explicit pipeline.
