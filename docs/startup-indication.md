# Startup indication

Git-on displays temporary progress while preparing the requested command.

## Display

- If the command has not started within 0.5 seconds of invocation, show an
  unobtrusive animated indicator. Measure the delay from invocation, including
  time spent before snapshot preparation.
- Keep the indicator active throughout preparation: local revision resolution,
  remote operations and network waits, verification, and snapshot preparation.
- Display it on one line in the terminal's text area beside the command text.
- Show useful Git transfer information as it becomes available: object counts,
  received bytes, transfer speed and pack processing. Use actual reported
  counts and a progress bar when the total is known. Do not invent percentages
  or replace these details with a generic loading message.
- While no transfer measurements are available, animate beside the requested
  revision. Do not create a sequence of status labels for brief internal calls.
- Use Unicode animation, highlighted labels and a colored progress bar. Respect
  disabled terminal colors. Fit revision details to the terminal's column
  width and escape embedded terminal controls.
- Display progress only on a TTY. Do not write indicator text or terminal
  control sequences to a redirected or non-TTY output stream.
- Draw through an independently opened nonblocking terminal. Preparation stays
  silent when Git-on cannot reopen stderr's terminal for writing.
- Keep startup shorter than 0.5 seconds silent. Successful preparation produces
  no warning or persistent banner.

## Terminal ownership

Git-on owns terminal interaction during preparation. Git and its helpers must
not independently write to the terminal. Interactive prompts must remain
usable, with indication suspended while necessary for interaction.
Input typed ahead remains available to the next prompt or requested command.
Restore terminal settings before suspending the job or starting the command.
Discard any pending indicator frame when the job resumes.
Clear a painted row only while Git-on still owns it.
The controlling shell can return the terminal before sending `SIGCONT`,
including after an uncatchable stop. Git-on gives up a painted row on this
signal to protect shell output. A continuation sent by the controller while
the job keeps foreground ownership can leave the previous indicator visible.
An uncatchable `SIGSTOP` can suspend drawing between checking ownership and
writing a frame. If the shell has changed the row by the time drawing resumes,
the pending write can reach that row. Git-on clears its row before a
cooperative `SIGTSTP`.

Once the requested command starts, it owns the terminal. Its standard streams
are inherited directly. Git-on must not capture or tee its output, consume its
input, or interfere with terminal job control.
Git-on reports preparation failures, demanded-object errors, and recovery
instructions on stderr. It bounds diagnostic buffering and lets cancellation
interrupt writes to a blocked destination. It does not capture or replay
native Git packet traces.

## Clearing

Clear the indicator's row immediately before starting the command while Git-on
owns it. The clear leaves the cursor on the row for the command's first output.

On preparation failure or cancellation, clear any row Git-on still owns before
showing genuine diagnostics. Drawing stops before command startup and stays
off during lazy object loading and final cleanup. Cancellation can interrupt
writes of Git-on diagnostics.
Git-on bounds the time spent drawing or clearing the indicator when the
terminal stops accepting output. Once the shell takes a row Git-on has used,
progress stays hidden for the remainder of preparation. A job started in the
background can show progress when it first enters the foreground. Discard a
pending frame on continuation or when a narrower terminal would wrap it.
Some terminal emulators reflow text already on the screen when the window
narrows. A reflowed fragment of an earlier frame can remain on another row.
Git-on clears only the active row it owns.
After drawing starts, a frame that cannot be cleared on an attached
foreground terminal within the time limit fails preparation. Preparation
continues if the terminal disconnects.
If the diagnostic writer fails to start, Git-on retries once and preserves the
original exit status. After a second failure it attempts to write the
diagnostic through stderr when a nonblocking write is available.
