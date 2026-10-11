# 0.1.1
Bump: patch

- Make the README easier to scan, with a compact installation command, practical examples, and links to detailed behavior and test guides.
- Remove obsolete pre-release guidance and use the standard `curl -fsSL … | sh` installation command. The bootstrap now uses curl's default redirect and TLS settings; the installer's truncated-script protection, SHA-256 verification, and atomic replacement remain in place.
- License git-on under MIT and include the license in release archives. The installer still accepts the original v0.1.0 archive layout.

CLI behavior and Linux/FUSE requirements are unchanged. No migration is needed.
