use crate::common::TestEnv;
use std::io::Write as _;
use std::process::{Output, Stdio};

pub fn decision(
    env: &TestEnv,
    input: &[u8],
    terminal: bool,
    args: &[&str],
) -> Result<Output, Box<dyn std::error::Error>> {
    let runner = r"
import os, pty, select, signal, subprocess, sys, termios, time

data = sys.stdin.buffer.read()
terminal = sys.argv[1] == 'tty'
master = slave = None
child = None
try:
    if terminal:
        master, slave = pty.openpty()
        assert os.isatty(slave)
        attrs = termios.tcgetattr(slave)
        attrs[3] &= ~termios.ECHO
        termios.tcsetattr(slave, termios.TCSANOW, attrs)
    child = subprocess.Popen(sys.argv[2:], stdin=slave if terminal else subprocess.PIPE,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             start_new_session=True)
    deadline = time.monotonic() + 8
    try:
        if terminal:
            os.close(slave)
            slave = None
            os.set_blocking(master, False)
            pending = memoryview(data + b'\n\x04')
            while pending:
                if not select.select([], [master], [], max(0, deadline - time.monotonic()))[1]:
                    raise subprocess.TimeoutExpired(child.args, 8)
                pending = pending[os.write(master, pending):]
        stdout, stderr = child.communicate(input=None if terminal else data,
                                           timeout=max(0, deadline - time.monotonic()))
    except subprocess.TimeoutExpired:
        os.killpg(child.pid, signal.SIGKILL)
        stdout, stderr = child.communicate()
        sys.stdout.buffer.write(stdout)
        sys.stderr.buffer.write(stderr + b'\nprocess timed out\n')
        sys.exit(124)
    sys.stdout.buffer.write(stdout)
    sys.stderr.buffer.write(stderr)
    sys.exit(child.returncode if child.returncode >= 0 else 128 - child.returncode)
finally:
    if child is not None and child.poll() is None:
        os.killpg(child.pid, signal.SIGKILL)
        child.wait()
    if master is not None:
        os.close(master)
    if slave is not None:
        os.close(slave)
";
    let mut command = env.process_for(std::path::Path::new("python3"));
    if terminal {
        command.env("DOTMAN_NON_INTERACTIVE", "");
    }
    let mut child = command
        .args([
            "-c",
            runner,
            if terminal { "tty" } else { "pipe" },
            env!("CARGO_BIN_EXE_dotman"),
        ])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let written = child.stdin.take().ok_or("stdin")?.write_all(input);
    if let Err(error) = written {
        child.kill()?;
        child.wait()?;
        return Err(error.into());
    }
    Ok(child.wait_with_output()?)
}
