# 1.5.2
<!-- Bump: patch -->

- Use the system libcurl on Apple Silicon macOS to reduce the download size.
  Native Git handles HTTPS when the system libcurl lacks TLS or HTTP/2.
