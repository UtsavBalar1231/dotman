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

## How a film is made

`films/<film>/film.json` names the machines, the `prepare` commands that run off camera, and the shots in order. A shot is a card, a diagram or a scene; `build.py` documents each.

- A card is the film's title card or the end card.
- A diagram (a JSON file beside `film.json`) lists boxes, optional titled groups and arrows, each with the time it appears; `diagram.html` documents the format.
- A scene (`scenes/*.json`) is the steps `record.py` documents: commands typed and run on one machine, and the answers typed when a question appears. A scene shot can end on a close-up: a hard cut to the row where a pattern first appears in the recording.

`build.py` starts one Alpine container per machine with no network, user `alex` and the machine's host name, sharing `/srv` for the remote. It records each scene through a pty sized to fill the terminal window that `window.json` places, so prompts, colours and dotman's own word wrap are what a user sees. `render.mjs` draws every shot frame by frame in headless Chromium: `term.html` replays a recording in the window, `card.html` draws a card and `diagram.html` builds a diagram. The shots are then joined with short dissolves, and the film fades to black at the end.

Every shot is set in Departure Mono, from `fonts/` (SIL OFL 1.1, see `fonts/LICENSE.txt`).

A scene fails when a command exits with a status it did not expect, or prints nothing for 15 seconds, which usually means a question with no answer; the error shows the last output.
