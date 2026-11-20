# 1.5.2
<!-- Bump: patch -->

- Use the system libcurl and its TLS and HTTP/2 libraries on macOS and Linux
  to reduce the download size.
- Linux binaries need `libcurl.so.4`. On Ubuntu the `curl` package installs
  this dependency. Native Git handles HTTPS when libcurl lacks TLS or HTTP/2.
