"""Private Linux child reaper for process_capture; invoked with Python -I -S.

This owns command descendants, including double forks and new sessions. It is
process lifecycle management, not a security boundary against hostile commands.
"""
from __future__ import annotations

import ctypes
import json
import os
import select
import signal
import subprocess
import sys
import time


def reap_tree(root: subprocess.Popen) -> None:
    """Keep ownership until waitpid proves every adopted child has been reaped.

    The caller bounds its wait and retains workspaces if cleanup takes too long.
    This reaper stays alive in that case, so children never lose their owner just
    because the caller's deadline expired (e.g. a child in uninterruptible I/O).
    """
    pause = 0.005
    while True:
        try:
            while True:
                pid, status = os.waitpid(-1, os.WNOHANG)
                if pid == 0:
                    break
                if pid == root.pid:
                    root.returncode = os.waitstatus_to_exitcode(status)
        except ChildProcessError:
            return
        try:
            with open(f"/proc/self/task/{os.getpid()}/children", encoding="ascii") as children:
                pids = [int(value) for value in children.read().split()]
            # These are our direct children; SIGCHLD is not ignored and there
            # is no other thread/handler calling waitpid. None of these PIDs
            # can be recycled between this snapshot and the signals below.
            for pid in pids:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        except OSError:
            # Retain ownership on a tracking/signal failure. The parent's
            # bounded wait will report unconfirmed cleanup and stop the run.
            pass
        time.sleep(pause)
        pause = min(pause * 2, 0.1)


def supervise(control: int, report: int, argv: list[str]) -> None:
    signal.signal(signal.SIGCHLD, signal.SIG_DFL)
    stopping = False

    def request_stop(_signal: int, _frame: object) -> None:
        nonlocal stopping
        stopping = True

    for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(number, request_stop)

    # The control pipe, not a kill of this reaper, requests cancellation.
    # Closing its parent endpoint also cancels when the trainer exits.
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(36, 1, 0, 0, 0) != 0:  # PR_SET_CHILD_SUBREAPER
        error = ctypes.get_errno()
        os.write(report, json.dumps({"errno": error}).encode())
        return
    try:
        root = subprocess.Popen(argv, close_fds=True)
    except OSError as error:
        os.write(report, json.dumps({"errno": error.errno}).encode())
        return
    try:
        while root.poll() is None and not stopping:
            ready, _, _ = select.select([control], [], [], 0.01)
            if ready:
                break
    finally:
        reap_tree(root)
    os.write(report, json.dumps({"returncode": root.returncode}).encode())


if __name__ == "__main__":
    control_fd, report_fd = map(int, sys.argv[1:3])
    try:
        supervise(control_fd, report_fd, sys.argv[3:])
    except BrokenPipeError:
        # The parent can leave after a cleanup deadline; cleanup above still
        # completes before this no-longer-consumed acknowledgement is sent.
        pass
    finally:
        os.close(control_fd)
        os.close(report_fd)
