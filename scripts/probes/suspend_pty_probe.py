#!/usr/bin/env python3
"""Pty probe for Ctrl+Z job control in the TUI (mini-agent-x6nc9).

Runs the TUI as a foreground job of an interactive bash on a pty, sends Ctrl+Z,
checks that bash reports the job as stopped and the terminal is cooked again
(alternate screen left), resumes with `fg`, and checks that the TUI repaints
and still takes input. Unix only. Usage: suspend_pty_probe.py <mini-agent>
"""

import os
import pty
import re
import fcntl
import select
import struct
import shutil
import signal
import sys
import tempfile
import termios
import time

LEAVE_ALT = b"\x1b[?1049l"
ENTER_ALT = b"\x1b[?1049h"


ANSI = re.compile(rb"\x1b\[[0-9;?<>]*[ -/]*[@-~]|\x1b\][^\x07]*\x07")


def read_text(fd, seconds):
    """Everything the terminal printed in `seconds`, escape sequences removed."""
    out = b""
    deadline = time.time() + seconds
    while time.time() < deadline:
        ready, _, _ = select.select([fd], [], [], 0.1)
        if ready:
            try:
                out += os.read(fd, 65536)
            except OSError:
                break
    return ANSI.sub(b"", out)


def read_until(fd, pattern, timeout, buffer=b""):
    deadline = time.time() + timeout
    while time.time() < deadline:
        ready, _, _ = select.select([fd], [], [], 0.1)
        if ready:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            buffer += chunk
            if re.search(pattern, buffer):
                return buffer
    raise SystemExit(f"timed out waiting for {pattern!r}; saw {buffer[-400:]!r}")


def main() -> int:
    binary = os.path.abspath(sys.argv[1])
    home = os.path.realpath(tempfile.mkdtemp(prefix="suspend-probe-"))
    env = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": home,
        "TERM": "xterm-256color",
        "OPENROUTER_API_KEY": "probe-key",
        "PS1": "PROBE$ ",
    }
    for name in ("CONFIG", "DATA", "LOCAL_DATA", "STATE", "CACHE", "CREDENTIALS"):
        path = os.path.join(home, name.lower())
        os.makedirs(path)
        env[f"MINI_AGENT_{name}_DIR"] = path
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(home)
        os.execve(
            "/bin/bash",
            ["bash", "--noprofile", "--norc", "-i"],
            env,
        )
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
    try:
        buf = read_until(fd, rb"PROBE\$ ", 10)
        os.write(fd, f"{binary} --no-session --no-context-files --no-sandbox\n".encode())
        buf = b""
        for _ in range(5):  # first-run questions are answered "no"
            buf = read_until(fd, rb"\[y/N\] |" + re.escape(ENTER_ALT), 20)
            if ENTER_ALT in buf:
                break
            os.write(fd, b"n\n")
            buf = b""
        time.sleep(1.0)  # let the first frame render
        os.write(fd, b"\x1a")  # Ctrl+Z
        buf = read_until(fd, rb"(?i)stopped", 10)
        assert LEAVE_ALT in buf, "terminal was not restored before stopping"
        os.write(fd, b"fg\n")
        buf = read_until(fd, re.escape(ENTER_ALT), 10)
        time.sleep(1.0)
        os.write(fd, b"hello")
        assert b"hello" in read_text(fd, 3), "the resumed prompt did not take input"
        print("ok: suspended, resumed with fg and the prompt takes input")
        return 0
    finally:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        shutil.rmtree(home, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
