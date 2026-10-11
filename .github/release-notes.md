# 0.1.0
Bump: initial

Initial Linux release of git-on.

- Run an arbitrary command against a pinned Git tree without changing the working tree.
- Load source directories and file contents on demand through FUSE.
- Keep tracked paths immutable; allow temporary writes only where the selected tree's .gitignore permits them.
- Inherit standard streams directly and optionally retain temporary writes with --keep.
- Install or update to the latest stable prebuilt release with one command, verified checksums, and atomic replacement.
- Include real-mount mutation/lifecycle checks and reproducible lazy-access benchmarks.

Requires Linux x86_64, glibc 2.35 or newer, the shared zlib/libgcc runtimes, a usable /dev/fuse and fusermount3. This is not a sandbox. See README for support boundaries and installation.
