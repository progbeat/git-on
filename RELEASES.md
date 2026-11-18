# 1.4.1
<!-- Bump: patch -->
<!-- Release-base: v1.4.0 -->

- Show temporary terminal progress after 0.5 seconds from invocation, including
  local resolution, remote network waits, verification, and snapshot preparation.
  Clear it before starting the command and on preparation failure or cancellation.
  Redirected output contains no indicator or terminal control sequences.
- Git-on owns the terminal during preparation and presents standard Git/SSH
  authentication prompts, pausing the animation for interaction.
  Configured askpass programs retain their original environment and selection
  policy. The requested command receives the original terminal and inherits
  standard streams directly.
