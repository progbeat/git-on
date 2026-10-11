# 1.1.0
Bump: minor

- Execute an HTTPS/SSH revision outside a checkout or select a configured remote: `git on origin main -- command args`. One operand still selects a local revision; two select repository and revision. Keep flags work on either side of REV; the command tail remains literal after `--`.
- Fetch exact objects on demand with a persistent cache bounded to 256 MiB and 4096 objects. Concurrent eviction preserves active run copies. Active temporary objects and ignored writes have no runner quota. No full checkout or full-clone fallback is introduced.
- Reuse an authenticated OpenSSH connection for lazy fetches. An unlocked agent needs no passphrase prompt; one selected encrypted identity authenticates before mounting. Lost connections and filter/OID rejection fail with precise diagnostics.
- Add `--verify` with native Git `verify-commit`/`verify-tag` on the exact resolved object and effective configuration, including inherited `git -c`. Git owns formats and trust. Failed verification forbids execution; signed tags and directory selectors remain bound to exact objects. Disable replacements consistently.

Remote execution requires Git 2.45.1+, a filtering server allowing requested OIDs and existing credentials. Remote ancestry/reflog/abbreviated-ID syntax, non-OpenSSH variants and SSH command shell prefixes/compounds are unsupported. Transparent `GIT_SSH` executables forwarding OpenSSH arguments are supported. Multiple encrypted identities or separately authenticated jump hosts may require extra initial interaction. See `docs/remote.md` for limits. Local syntax and filesystem policy remain compatible with 1.0.0. Verification does not certify tests or provide sandbox safety.
