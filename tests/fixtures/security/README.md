`sha-mbles-1.bin` is the 640-byte public SHA-1 collision test vector distributed
with RustCrypto sha1-checked 0.10.0 (`tests/data/sha-mbles-1.bin`). It is inert test
data and is never executed. The regression requires the same collision-detection
class of defense used by native Git, rather than trusting a plain SHA-1 digest.

Upstream: https://github.com/RustCrypto/hashes/tree/e766aecfe34f29a5628fa9047775b2eb8e713aa9/sha1-checked
Crate license: MIT OR Apache-2.0. Research: https://sha-mbles.github.io/
