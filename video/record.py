"""Records a scene of the real dotman binary, run in a demo machine through a pty, as an asciicast v2 file.

A scene is {"on": machine, "cols", "rows", "steps"}; each step is one of
  {"hidden": cmd}                    run on the machine, not recorded; fails the scene if it fails
  {"say": text}                      type a comment line at the prompt
  {"run": cmd, "answers": [[seen, typed], ...], "status": 0}
                                     type cmd, run it, type each answer once `seen` has been printed
  {"wait": seconds}                  hold the screen

usage: record.py SCENE.json OUT.cast CONTAINER
"""

import codecs
import fcntl
import json
import os
import select
import struct
import subprocess
import sys
import termios
import time

TYPE_DELAY = 0.045
MAX_GAP = 0.6
IDLE_LIMIT = 15
PROMPT = "\x1b[1;32malex@{host}\x1b[0m:\x1b[1;34m~\x1b[0m$ "


class SceneError(Exception):
    pass


def docker_exec(container, cmd, tty):
    return ["docker", "exec", *(["-it"] if tty else []), "-u", "alex", "-w", "/home/alex",
            "-e", "TERM=xterm-256color", container, "bash", "-c", cmd]


class Cast:
    def __init__(self, cols, rows):
        self.events, self.t = [], 0.0
        self.header = {"version": 2, "width": cols, "height": rows, "env": {"TERM": "xterm-256color"}}

    def out(self, text, gap=0.0):
        self.t += gap
        self.events.append([round(self.t, 3), "o", text])

    def type(self, text):
        for ch in text:
            self.out(ch, TYPE_DELAY)

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
    try:
        while True:
            ready, _, _ = select.select([master], [], [], 0.05)
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
            if pending and pending[0][0] in seen:
                _, typed = pending.pop(0)
                seen = ""
                time.sleep(0.4)
                for ch in typed:
                    os.write(master, ch.encode())
                    time.sleep(TYPE_DELAY)
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
    cast = Cast(cols, rows)
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
            cast.out("\r\n", 0.3)
            code = run(cast, container, step["run"], step.get("answers", []), cols, rows)
            if code != step.get("status", 0):
                raise SceneError(f"`{step['run']}` exited {code}, expected {step.get('status', 0)}")
            cast.out(prompt, 0.4)
        elif "wait" in step:
            cast.out("", step["wait"])
    cast.out("", 1.5)
    cast.save(out)


if __name__ == "__main__":
    scene_path, out_path, machine = sys.argv[1:4]
    with open(scene_path) as f:
        loaded = json.load(f)
    try:
        record(loaded, machine, out_path)
    except SceneError as e:
        sys.exit(str(e))
