# Demo films

Each film in `films/` shows the real `dotman` binary: its terminal scenes are recordings of it running on throwaway machines.

```bash
just video                 # all films
just video setup daily     # some of them
```

Each film lands in `target/video/<film>.mp4`, with `<film>-sheet.png`, 24 frames spread over the film, for review. The casts and per-shot clips are in `target/video/<film>/`.

## What it needs

- Docker, Python 3, Node, ffmpeg and Chromium. `CHROMIUM` names the browser if it is not at `/usr/bin/chromium`.
- The `x86_64-unknown-linux-musl` Rust target and `musl-gcc`, for the static binary the machines run. `DOTMAN_BIN` names another one.
- The [huashu-art-motion](https://github.com/alchaincyf/huashu-art-motion) engine (MIT) for the concept clips, checked out at commit `1468196` into `.claude/skills/huashu-art-motion`, or wherever `HUASHU_ENGINE` points at its `scripts/engine`.

## How a film is made

`films/<film>/film.json` names the machines, the `prepare` commands that run off camera, and the shots in order. A shot is a clip or a scene.

- A clip is a huashu-art-motion spec (`clips/*.json`); `huashu-art-motion/scripts/engine/clip_schema.json` lists its grammars.
- A scene (`scenes/*.json`) is the steps `record.py` documents: commands typed and run on one machine, and the answers typed when a question appears.

`build.py` starts one Alpine container per machine with no network, user `alex` and the machine's host name, sharing `/srv` for the remote. It records each scene through a pty, so prompts and colours are what a user sees. `render.mjs` draws every shot frame by frame in headless Chromium: `term.html` replays a recording, and huashu's `clip.html` draws a clip. The shots are then crossfaded into one file.

Every shot is set in Departure Mono, from `fonts/` (SIL OFL 1.1, see `fonts/LICENSE.txt`); `render.mjs` swaps it in for the fonts huashu's grammars name.

A scene fails when a command exits with a status it did not expect, or prints nothing for 15 seconds, which usually means a question with no answer; the error shows the last output.
