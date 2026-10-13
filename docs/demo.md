# Terminal demo

The README displays an animated WebP converted losslessly from the VHS GIF.
The checked-in provenance records both formats and the published media commit.

The README shows the original [SL by Toyoda Masashi](https://github.com/mtoyoda/sl)
running through git-on. To reproduce the reviewed version, use its full commit ID:

```sh
git on https://github.com/mtoyoda/sl.git 923e7d7ebc5c1f009755bdeb789ac25658ccce03 -- sh -c 'make && ./sl'
```

The selected repository's `.gitignore` contains `/sl`. Its genuine `make` command
builds that ignored executable, so the default `ignored` policy suffices. The
train runs to completion and returns to the shell. git-on unmounts and deletes
the temporary snapshot, including the generated `sl`. No executable is written
to the user's checkout or a separate host path.

## Source and dependencies

The reviewed external commit is
[`923e7d7ebc5c1f009755bdeb789ac25658ccce03`](https://github.com/mtoyoda/sl/tree/923e7d7ebc5c1f009755bdeb789ac25658ccce03),
with tree `56a5fd07310088f1e6372d81455c8489c66ea00b`. Review its `Makefile`, `sl.c`,
`sl.h` and `.gitignore` before executing it. The recording uses that commit ID,
so a later upstream branch change does not change its source.
The recording first types `master`, erases those six characters, and replaces
them with the full commit ID before pressing Enter. Only the pinned source is
executed. A pin fixes the version; it does not establish that the code is safe.

Running the example needs Linux, a usable `/dev/fuse`, `fusermount3`, native Git
2.45.1+, `make`, GCC and ncurses development headers/libraries. Recording also
needs [VHS 0.12.1](https://github.com/charmbracelet/vhs/releases/tag/v0.12.1),
`ttyd`, FFmpeg, Chromium and the Source Code Pro font. Check the dependencies
before recording. The recording helper installs nothing and changes no shell
profile, device permission or system configuration.

Copyright 1993,1998,2014 Toyoda Masashi (mtoyoda@acm.org).
The upstream [license notice](../assets/SL-LICENSE) accompanies the animation.

## Reproduce and inspect

[`assets/demo.tape`](../assets/demo.tape) types the actual command into a clean
Bash terminal in the selected Neon style: near-black background, restrained
turquoise text, a purple prompt, Source Code Pro at 24 px, and a compact rounded
window. Its 54-column terminal wraps the complete command instead of shrinking
the text. Every character, including spaces, URLs and the hash, uses the same
45 ms baseline with independent noise between -8 and +8 ms. The committed tape
fixes that sequence with seed 923; it has no typing-speed ramps or paste bursts.
It pauses at the revision edit and leaves 1.2 seconds to read the completed
command before running it. It waits for the real final prompt, then clears the
screen so the loop rejoins the initial prompt. It contains one animation, the
steam locomotive. VHS renders the real terminal and original program.

```sh
cargo build --release --locked
python3 scripts/record_demo.py --binary target/release/git-on --output target/sl-recording
uv run --no-project --with pillow==11.3.0 python scripts/convert_demo.py --gif target/sl-recording/demo.gif --output target/sl-recording/demo.webp
```

Choose a new or empty output directory: the helper refuses any existing
contents before recording. During execution it checks the live FUSE recovery
state against the reviewed tree, observes the generated executable inside the
mount, and records the shell's real exit status through a private prompt hook.
Afterward it checks that the run and executable were removed. This status hook
does not modify the displayed command or output. `demo.json` records the actual
host, time, Git version, git-on binary hash, VHS version, tape hash, external
commit/tree, cleanup result, GIF size and SHA-256. Relative tape outputs,
screenshots, the MP4 and ASCII recording stay in the requested output directory.
`vhs.log` contains recorder diagnostics. The VHS settings and `PS1` affect only
the recording process, never the user's terminal configuration.
The checked-in provenance additionally records the release build's source hash
and per-file input hashes, final optimized GIF hash and visual inspection results.

Open the resulting GIF and inspect the complete command, compiler output,
train motion and final prompt. Keep normal playback speed, readable type and a
short duration. The helper rejects GIFs over 5 MiB; aim below 3 MiB. When the GIF
is unsuitable for repository storage, keep it local, do not commit it, and do
not create the associated PR. Publish the verified WebP in the separate
`demo-media` branch. The tape, provenance and upstream license stay in `assets/`
on the main development branch. WebP conversion needs Pillow 11.3.0 in an
isolated Python environment; the command above uses uv. It verifies identical decoded RGB,
accumulated frame durations, dimensions and loop settings before accepting output.

The selected GIF is losslessly optimized with `gifsicle --careful -O3`.
Verification compares decoded frame colors and durations, dimensions and loop
settings before and after optimization, and inspects the beginning, revision
edit, full command, moving train, final prompt and loop seam. Check the complete
command at README width and at 375 px as well. The other style candidates stay
in ignored local recording directories; only the selected animation is published.

## Media storage

The README links to the verified animation at a specific commit in `demo-media`.
That branch has a parentless commit containing only `demo.webp` and `SL-LICENSE`;
it is never merged into master. Future recordings can use another parentless
media commit and update the media branch and README/provenance references.
This keeps animation files out of master's history. The media branch still
belongs to the same Git repository and ordinary clones fetch branches by default.

Replacing the branch reference does not guarantee deletion of older objects on
GitHub. Pull-request references, caches and forks can retain them; see
[GitHub's history-removal limits](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/removing-sensitive-data-from-a-repository).
