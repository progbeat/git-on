# Contributor and agent instructions

Read README.md and docs/specification.md before changing behavior. Keep the
implementation generic: application-specific Python/uv/Numba examples are not
filesystem exceptions. Preserve the complete FUSE-operation coverage matrix and
its guard whenever callbacks or tests change.

## Every pull request is a release

Follow docs/releases.md. Fetch current master, choose patch/minor/major from the
actual compatibility impact, update Cargo.toml and Cargo.lock together, and write
the new version's user-facing .github/release-notes.md. Fixes also need a bump.
Do not create a manual tag, release, merge, or visibility change unless requested.
Updating the PR is not permission to merge it. If master advances, update the
branch and recompute the version before requesting review.

Run formatting, strict clippy, Rust tests, the FUSE coverage guard, release-policy
tests and integration tests. Real FUSE checks must run on a host with /dev/fuse;
a skip is not a pass. Verify CI at the exact final head. Do not claim all protocol
operations or performance requirements are verified from a green subset.

Release artifacts are generated only by the master workflow after all checks.
Document changes in GitHub Release notes rather than maintaining a mandatory
CHANGELOG. Never insert secrets or access tokens into notes, logs or fixtures.
