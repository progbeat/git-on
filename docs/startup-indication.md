# Startup indication

Git-on displays temporary progress while preparing the requested command.

## Display

- If the command has not started within 0.5 seconds of invocation, show an
  unobtrusive animated indicator. Measure the delay from invocation, including
  time spent before snapshot preparation.
- Keep the indicator active throughout preparation: local revision resolution,
  remote operations and network waits, verification, and snapshot preparation.
- Display it in the terminal's text area beside the command text, not in the
  terminal title.
- Use a smooth Unicode dot spinner with a cyan accent and a bold status label.
  When terminal colors are disabled, display the spinner and label without
  highlighting.
- Display progress only on a TTY. Do not write indicator text or terminal
  control sequences to a redirected or non-TTY output stream.
- Keep startup shorter than 0.5 seconds silent. Successful preparation produces
  no warning or persistent banner.

## Terminal ownership

Git-on owns terminal interaction during preparation. Git and its helpers must
not independently write to the terminal. Interactive prompts must remain
usable, with indication suspended while necessary for interaction.
Input typed ahead remains available to the next prompt or requested command.
Restore terminal settings before suspending the job or starting the command.

Once the requested command starts, it owns the terminal. Its standard streams
are inherited directly. Git-on must not capture or tee its output, consume its
input, or interfere with terminal job control.

## Clearing

Remove every trace of the indicator immediately before starting the command.
Leave no blank line, banner, spinner, or other artifact. The command runs as if
no indicator had been displayed.

On preparation failure or cancellation, clear the indicator without hiding
genuine diagnostics. Never display it after command startup, including during
lazy object loading or final cleanup.
