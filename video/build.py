"""Builds one demo film: fresh demo machines, every scene recorded from the real binary, every shot
rendered, and the shots crossfaded into target/video/<film>.mp4 with a contact sheet beside it.

usage: build.py FILM_DIR

FILM_DIR/film.json is {"machines": [names], "prepare": [{"on": machine, "run": cmd}], "fade": seconds,
"shots": [{"clip": path} | {"scene": path}]}. `prepare` runs off camera, in order, before the first shot.
A clip is a huashu-art-motion spec; a scene is a record.py scene whose "on" names a machine.
DOTMAN_BIN names a static Linux dotman (default: the musl release build).
"""

import json
import os
import subprocess
import sys
from pathlib import Path

import record

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
IMAGE = "dotman-demo"


def sh(*cmd, **kw):
    return subprocess.run(cmd, check=True, **kw)


def duration(path):
    out = sh("ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", str(path),
             capture_output=True, text=True).stdout
    return float(out)


def machines_up(film, names, binary):
    volume = f"{IMAGE}-{film}-remote"
    subprocess.run(["docker", "volume", "rm", "-f", volume], capture_output=True)
    sh("docker", "volume", "create", volume, capture_output=True)
    containers = {}
    for name in names:
        container = f"{IMAGE}-{film}-{name}"
        subprocess.run(["docker", "rm", "-f", container], capture_output=True)
        sh("docker", "run", "-d", "--name", container, "--hostname", name, "--network", "none",
           "-v", f"{volume}:/srv", "-v", f"{binary}:/usr/local/bin/dotman:ro", IMAGE, "sleep", "infinity",
           capture_output=True)
        containers[name] = container
    return volume, containers


def machines_down(volume, containers):
    for container in containers.values():
        subprocess.run(["docker", "rm", "-f", container], capture_output=True)
    subprocess.run(["docker", "volume", "rm", "-f", volume], capture_output=True)


def join(parts, fade, out):
    inputs, chain, label, at = [], [], "[s0]", duration(parts[0])
    for i, part in enumerate(parts):
        inputs += ["-i", str(part)]
        chain.append(f"[{i}:v]settb=AVTB,fps=30,format=yuv420p[s{i}]")
    for i, part in enumerate(parts[1:], start=1):
        chain.append(f"{label}[s{i}]xfade=transition=fade:duration={fade}:offset={at - fade:.3f}[x{i}]")
        label, at = f"[x{i}]", at + duration(part) - fade
    sh("ffmpeg", "-y", "-v", "error", *inputs, "-filter_complex", ";".join(chain), "-map", label,
       "-c:v", "libx264", "-preset", "slow", "-crf", "18", "-pix_fmt", "yuv420p", "-movflags", "+faststart", str(out))


def main():
    film_dir = Path(sys.argv[1]).resolve()
    film = film_dir.name
    spec = json.loads((film_dir / "film.json").read_text())
    built = Path(os.environ.get("CARGO_TARGET_DIR", REPO / "target")) / "x86_64-unknown-linux-musl/release/dotman"
    binary = Path(os.environ.get("DOTMAN_BIN", built)).resolve()
    if not binary.is_file():
        sys.exit(f"no dotman binary at {binary}; run `just video`, which builds it, or set DOTMAN_BIN")
    out_dir = REPO / "target/video" / film
    out_dir.mkdir(parents=True, exist_ok=True)
    sh("docker", "build", "-q", "-t", IMAGE, "-f", str(HERE / "Containerfile"), str(HERE), capture_output=True)
    volume, containers = machines_up(film, spec["machines"], binary)
    parts = []
    try:
        for step in spec.get("prepare", []):
            done = subprocess.run(record.docker_exec(containers[step["on"]], step["run"], False),
                                  capture_output=True, text=True)
            if done.returncode:
                sys.exit(f"prepare on {step['on']} failed: {step['run']}\n{done.stdout}{done.stderr}")
        for n, shot in enumerate(spec["shots"], start=1):
            if "scene" in shot:
                scene_path = film_dir / shot["scene"]
                scene = json.loads(scene_path.read_text())
                stem = f"{n:02d}-{scene_path.stem}"
                cast = out_dir / f"{stem}.cast"
                try:
                    record.record(scene, containers[scene["on"]], cast)
                except record.SceneError as e:
                    sys.exit(f"{scene_path.relative_to(REPO)}: {e}")
                term = out_dir / f"{stem}.json"
                term.write_text(json.dumps({"cast": str(cast), "title": f"alex@{scene['on']}: ~"}))
                kind, clip = "term", term
            else:
                kind, clip = "huashu", film_dir / shot["clip"]
                stem = f"{n:02d}-{clip.stem}"
            part = out_dir / f"{stem}.mp4"
            sh("node", str(HERE / "render.mjs"), kind, str(clip), str(part))
            parts.append(part)
            print(f"{stem}: {duration(part):.1f}s", flush=True)
    finally:
        machines_down(volume, containers)
    final = REPO / "target/video" / f"{film}.mp4"
    join(parts, spec.get("fade", 0.4), final)
    sheet = REPO / "target/video" / f"{film}-sheet.png"
    sh("ffmpeg", "-y", "-v", "error", "-i", str(final), "-vf",
       f"fps=24/{duration(final):.3f},scale=640:-1,tile=4x6:padding=8", "-frames:v", "1", "-update", "1", str(sheet))
    print(f"{final.relative_to(REPO)}: {duration(final):.1f}s, sheet {sheet.relative_to(REPO)}")


if __name__ == "__main__":
    main()
