# 1.4.2
<!-- Bump: patch -->

- Show live Git object counts, received bytes, transfer speed and pack progress
  in a highlighted terminal display that clears before the command starts.
  Respect disabled colors and keep redirected output free of progress text.
- Fetch remote materialized snapshots in one shallow transfer instead of
  repeated directory and object fetches. Keep lazy execution demand driven
  and preserve bounded transfers under finite per-file resource limits.
