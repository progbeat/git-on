# Contributor and agent instructions

Read README.md and docs/specification.md before changing behavior. Keep the
implementation generic: application-specific Python/uv/Numba examples are not
filesystem exceptions. Preserve the complete FUSE-operation coverage matrix and
its guard whenever callbacks or tests change.

docs/interview.md records the author's intent; keep the implementation consistent
with it. Read the project documentation and interview before asking questions.
If the documentation leaves a material ambiguity about the author's intent or
requirements, ask the author a self-contained question before making the affected
implementation decision.
Record the complete question and answer in docs/interview.md in English without
changing their meaning. Verify technical facts independently;
do not invent author decisions or treat the current implementation as proof of
intent. Recorded goals are not evidence that they are already implemented.

## Release changes to the binary

Follow docs/releases.md. Fetch current master. Changes to CLI source, runtime
dependencies or binary build settings need exactly one patch/minor/major bump
from the actual compatibility impact, matching Cargo.toml and Cargo.lock, and
user-facing RELEASES.md. Behavioral fixes also need a bump.
Documentation, GIFs, tests, standalone examples and install.sh alone keep the
current master version and skip binary publication. Required checks still run.
Do not create a manual tag, release, merge, or visibility change unless requested.
Updating the PR is not permission to merge it. If master advances, update the
branch and recompute any required version before requesting review.

Run formatting, strict clippy, Rust tests, the FUSE coverage guard, release-policy
tests and integration tests. Real FUSE checks must run on a host with /dev/fuse;
a skip is not a pass. Verify CI at the exact final head. Do not claim all protocol
operations or performance requirements are verified from a green subset.

After master checks pass, a version trigger starts the generated cargo-dist
workflow at the tested commit's tag. PR package checks never publish. Keep the
current notes in RELEASES.md for cargo-dist to publish to GitHub; release history
need not be maintained in the repository. Never insert secrets or access tokens
into notes, logs or fixtures.

Every behavioral bug discovered in review must get a focused regression test.
Demonstrate that it reproduces the old failure, then passes after the fix; keep
that test in the permanent suite. Record red-before/green-after evidence instead
of treating a code change or an unrelated passing suite as sufficient verification.
