import errno
import fcntl
import os
import pty
import select
import signal
import struct
import sys
import termios
import time

input_path, columns, seconds, *command = sys.argv[1:]
with open(input_path, "rb") as source:
    pending = source.read() + b"\x04\x04"

pid, master = pty.fork()
if pid == 0:
    if int(columns):
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 24, int(columns), 0, 0))
    os.execvp(command[0], command)

deadline = time.monotonic() + float(seconds)
os.set_blocking(master, False)
closed = False
reaped = False
try:
    while True:
        # Keep the group leader's PID reserved while a descendant can hold the PTY.
        if closed:
            reaped = os.waitpid(pid, os.WNOHANG)[0] == pid
            if reaped:
                break
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            print(f"terminal command timed out: {command!r}", file=sys.stderr)
            sys.exit(124)
        readable, writable, _ = select.select(
            [] if closed else [master], [master] if pending and not closed else [], [],
            min(remaining, 0.05) if closed else remaining,
        )
        if writable:
            try:
                pending = pending[os.write(master, pending):]
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                pending = b""
        if readable:
            try:
                data = os.read(master, 65536)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                closed = True
                continue
            if not data:
                closed = True
                continue
            sys.stdout.buffer.write(data)
finally:
    if not closed or not reaped:
        try:
            os.killpg(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    os.close(master)
    if not reaped:
        os.waitpid(pid, 0)
