"""Records a scene of the real dotman binary, run in a demo machine through a pty, as an asciicast v2 file.

A scene is {"on": machine, "cols", "rows", "seed", "steps"}; each step is one of
  {"hidden": cmd}                    run on the machine, not recorded; fails the scene if it fails
  {"say": text}                      type a comment line at the prompt
  {"run": cmd, "answers": [[seen, typed], ...], "status": 0}
                                     type cmd, run it, type each answer once `seen` has been printed
  {"wait": seconds}                  hold the screen

Keys are typed at a human cadence drawn from `seed`, so the same scene types the same way every time.

usage: record.py SCENE.json OUT.cast CONTAINER
"""

import codecs
import fcntl
import json
import os
import random
import select
import struct
import subprocess
import sys
import termios
import time

MAX_GAP = 0.5
HOLD = 1.2
IDLE_LIMIT = 15
PROMPT = "\x1b[1;32malex@{host}\x1b[0m:\x1b[1;34m~\x1b[0m$ "


class SceneError(Exception):
    pass


def docker_exec(container, cmd, tty):
    return ["docker", "exec", *(["-it"] if tty else []), "-u", "alex", "-w", "/home/alex",
            "-e", "TERM=xterm-256color", container, "bash", "-c", cmd]


class Typist:
    def __init__(self, seed):
        self.rng, self.prev = random.Random(seed), " "

    def delay(self, ch):
        u = self.rng.uniform
        if ch in "\r\n":
            d = u(0.35, 0.5)
        elif self.prev in " /":
            d = u(0.08, 0.12)
        elif self.prev.isalnum() and ch.isalnum():
            d = u(0.04, 0.056)
        else:
            d = u(0.045, 0.07)
        self.prev = ch
        return d


class Cast:
    def __init__(self, cols, rows, typist):
        self.events, self.t, self.typist = [], 0.0, typist
        self.header = {"version": 2, "width": cols, "height": rows, "env": {"TERM": "xterm-256color"}}

    def out(self, text, gap=0.0):
        self.t += gap
        self.events.append([round(self.t, 3), "o", text])

    def type(self, text):
        for ch in text:
            self.out(ch, self.typist.delay(ch))

    def save(self, path):
        with open(path, "w") as f:
            f.write(json.dumps(self.header) + "\n")
            for e in self.events:
                f.write(json.dumps(e) + "\n")


def run(cast, container, cmd, answers, cols, rows):
    master, slave = os.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    pid = os.fork()
    if pid == 0:
        os.close(master)
        os.login_tty(slave)
        os.execvp("docker", docker_exec(container, cmd, True))
    os.close(slave)
    decoder = codecs.getincrementaldecoder("utf-8")("replace")
    pending, seen, last = list(answers), "", time.monotonic()
    keys, next_key = [], 0.0
    try:
        while True:
            wait = min(0.05, max(0.0, next_key - time.monotonic())) if keys else 0.05
            ready, _, _ = select.select([master], [], [], wait)
            if ready:
                try:
                    data = os.read(master, 4096)
                except OSError:
                    break
                if not data:
                    break
                now = time.monotonic()
                text = decoder.decode(data)
                cast.out(text, min(now - last, MAX_GAP))
                last, seen = now, seen + text
            if time.monotonic() - last > IDLE_LIMIT:
                raise SceneError(f"`{cmd}` printed nothing for {IDLE_LIMIT}s; it may wait on a prompt "
                                 f"with no answer. Last output: {seen[-300:]!r}")
            # Keys go in one at a time between reads, so an editor's echo is recorded as it is typed.
            if not keys and pending and pending[0][0] in seen:
                _, typed = pending.pop(0)
                seen, keys = "", list(typed)
                next_key = time.monotonic() + 0.4 + cast.typist.delay(keys[0])
            if keys and time.monotonic() >= next_key:
                os.write(master, keys.pop(0).encode())
                if keys:
                    next_key = time.monotonic() + cast.typist.delay(keys[0])
    except SceneError:
        os.kill(pid, 9)
        subprocess.run(["docker", "exec", "-u", "root", container, "pkill", "-x", "dotman"], capture_output=True)
        raise
    finally:
        os.close(master)
    _, status = os.waitpid(pid, 0)
    if pending:
        raise SceneError(f"`{cmd}` ended before printing {pending[0][0]!r}. Last output: {seen[-300:]!r}")
    return os.waitstatus_to_exitcode(status)


def record(scene, container, out):
    cols, rows = scene.get("cols", 100), scene.get("rows", 24)
    host = subprocess.run(["docker", "exec", container, "hostname"], capture_output=True, text=True,
                          check=True).stdout.strip()
    prompt = PROMPT.format(host=host)
    cast = Cast(cols, rows, Typist(scene.get("seed", 1)))
    cast.out(prompt)
    for step in scene["steps"]:
        if "hidden" in step:
            done = subprocess.run(docker_exec(container, step["hidden"], False), capture_output=True, text=True)
            if done.returncode:
                raise SceneError(f"hidden step failed: {step['hidden']}\n{done.stderr}")
        elif "say" in step:
            cast.out("\x1b[2m")
            cast.type(step["say"])
            cast.out("\x1b[0m\r\n" + prompt, 0.8)
        elif "run" in step:
            cast.type(step["run"])
            cast.out("\r\n", cast.typist.delay("\r"))
            code = run(cast, container, step["run"], step.get("answers", []), cols, rows)
            if code != step.get("status", 0):
                raise SceneError(f"`{step['run']}` exited {code}, expected {step.get('status', 0)}")
            cast.out(prompt, 0.1)
            cast.out("", HOLD)
        elif "wait" in step:
            cast.out("", step["wait"])
    cast.save(out)


if __name__ == "__main__":
    scene_path, out_path, machine = sys.argv[1:4]
    with open(scene_path) as f:
        loaded = json.load(f)
    try:
        record(loaded, machine, out_path)
    except SceneError as e:
        sys.exit(str(e))
