# 1.5.2
<!-- Bump: patch -->

- Use the operating system's libcurl for HTTPS on macOS and Linux. This reduces
  the download size and lets the platform supply updates for its TLS stack.
- Linux binaries require `libcurl.so.4` from the distribution. On Ubuntu the
  `curl` package installs it. macOS supplies libcurl with the system.
