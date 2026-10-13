# Run through scripts/record_demo.py to validate the source pin and FUSE cleanup.
# Neon: master is edited before execution; only the reviewed SHA runs.
# Upstream: mtoyoda/sl @ 923e7d7ebc5c1f009755bdeb789ac25658ccce03
Output demo.gif
Output demo.mp4
Output demo.ascii

Require git
Require git-on
Require make
Require gcc

Set Shell bash
Set FontFamily "Source Code Pro,DejaVu Sans Mono,monospace"
Set FontSize 24
Set Columns 54
Set Rows 20
Set LineHeight 1.0
Set Padding 20
Set Margin 10
Set MarginFill "#171528"
Set WindowBar Rings
Set WindowBarSize 28
Set BorderRadius 12
Set Theme {"black": "#24272d", "red": "#dd777c", "green": "#85c8a4", "yellow": "#dcc387", "blue": "#7f9edb", "magenta": "#c397ef", "cyan": "#59d9c7", "white": "#dadde2", "brightBlack": "#70747d", "brightRed": "#ec979a", "brightGreen": "#c5d5a9", "brightYellow": "#ead6ac", "brightBlue": "#abc0df", "brightMagenta": "#d0b9df", "brightCyan": "#a3d2d6", "brightWhite": "#e1faf4", "name": "Neon", "background": "#080c11", "foreground": "#c0e7e2", "cursor": "#c397ef", "selection": "#171528"}
Set Framerate 25
Set CursorBlink false
Set TypingSpeed 45ms
Set WaitTimeout 60s
Env PS1 `\[\e[38;2;195;151;239m\]$ \[\e[0m\]`

# Ordinary characters and Backspace use 45ms +/- 8ms independent noise.
# Fixed generation seed 923; spaces and URL keep the same timing rule.
# The full hash appears instantly as one visible chunk after master is erased.
Sleep 450ms
Screenshot initial.png
Type@52ms "g"
Type@41ms "i"
Type@48ms "t"
Type@37ms " "
Type@50ms "o"
Type@39ms "n"
Type@40ms " "
Type@51ms "h"
Type@51ms "t"
Type@43ms "t"
Type@51ms "p"
Type@44ms "s"
Type@40ms ":"
Type@49ms "/"
Type@51ms "/"
Type@52ms "g"
Type@51ms "i"
Type@38ms "t"
Type@53ms "h"
Type@37ms "u"
Type@39ms "b"
Type@43ms "."
Type@39ms "c"
Type@51ms "o"
Type@38ms "m"
Type@41ms "/"
Type@41ms "m"
Type@46ms "t"
Type@38ms "o"
Type@46ms "y"
Type@51ms "o"
Type@42ms "d"
Type@51ms "a"
Type@46ms "/"
Type@40ms "s"
Type@50ms "l"
Type@39ms "."
Type@44ms "g"
Type@51ms "i"
Type@48ms "t"
Type@40ms " "
Type@38ms "m"
Type@47ms "a"
Type@37ms "s"
Type@38ms "t"
Type@48ms "e"
Type@48ms "r"
Screenshot branch.png
# Deliberate revision-edit hold, outside the typing cadence.
Sleep 650ms
Set TypingSpeed 51ms
Backspace
Set TypingSpeed 53ms
Backspace
Set TypingSpeed 38ms
Backspace
Set TypingSpeed 49ms
Backspace
Set TypingSpeed 39ms
Backspace
Set TypingSpeed 40ms
Backspace
# Brief visible pause before the paste gesture.
Sleep 200ms
# Deliver the complete hash as one zero-delay input chunk while recording is paused.
# Resume only after the real terminal has echoed the whole ID and rendered it.
# This emulates the visible effect of a paste without using the OS clipboard.
Hide
Type@0ms "923e7d7ebc5c1f009755bdeb789ac25658ccce03"
Wait+Line /58ccce03$/
Sleep 80ms
Show
Sleep 100ms
Screenshot pinned.png
Type@39ms " "
Type@48ms "-"
Type@42ms "-"
Type@52ms " "
Type@46ms "s"
Type@40ms "h"
Type@53ms " "
Type@41ms "-"
Type@38ms "c"
Type@37ms " "
Type@47ms "'"
Type@44ms "m"
Type@44ms "a"
Type@51ms "k"
Type@41ms "e"
Type@38ms " "
Type@41ms "&"
Type@47ms "&"
Type@50ms " "
Type@46ms "."
Type@40ms "/"
Type@48ms "s"
Type@49ms "l"
Type@51ms "'"
Wait+Line /'make && \.\/sl'$/
Screenshot command.png
# Reading hold after the complete command, outside the typing cadence.
Sleep 1200ms
Enter
Sleep 450ms
Wait+Screen /(?m)^ {6}_D _\|  \|_______\//
Screenshot comparison.png
Sleep 80ms
Wait+Line /^\$ ?$/
Sleep 900ms
Screenshot exit.png
Sleep 80ms
Ctrl+L
Sleep 350ms
Screenshot loop.png
Sleep 80ms
