"""Bounded diagnostic output capture shared by the Gym trainer and task miner.

Results normally contain diagnostic byte tails. A stdout limit selects complete
capture with an overflow sentinel for callers such as the miner's Git blob reader.
"""

from __future__ import annotations

import json
import math
import os
import selectors
import subprocess
import sys
import time
from pathlib import Path

OUTPUT_TAIL_BYTES = 2000
PROCESS_REAP_TIMEOUT_SECS = 5


class ProcessCleanupError(RuntimeError):
    """Cleanup was not confirmed; callers must stop and preserve workspaces."""


class _OwnedProcess:
    def __init__(self, argv: list[str], cwd: Path, env: dict[str, str]) -> None:
        self.argv = argv
        self.cwd = cwd
        self.control: int | None = None
        self.report: int | None = None
        self.result: dict[str, int] | None = None
        self.cleanup_started = False
        if sys.platform != "linux":
            self.process = subprocess.Popen(argv, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            return
        descriptors: list[int] = []
        try:
            control_read, control_write = os.pipe()
            descriptors.extend((control_read, control_write))
            report_read, report_write = os.pipe()
            descriptors.extend((report_read, report_write))
            # Complete fallible pipe setup before a command can start.
            os.set_blocking(report_read, False)
            supervisor = str(Path(__file__).with_name("_process_supervisor.py").resolve())
            self.process = subprocess.Popen(
                [sys.executable, "-I", "-S", supervisor, str(control_read), str(report_write), *argv],
                cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                pass_fds=(control_read, report_write),
            )
        except BaseException as error:
            for descriptor in descriptors:
                os.close(descriptor)
            if isinstance(error, (KeyboardInterrupt, SystemExit)):
                # Popen can be interrupted after creating the supervisor.
                # Closing the control pipe requests cleanup, but we have no
                # returned process handle with which to confirm completion.
                raise ProcessCleanupError(
                    f"Gym owner startup interrupted; stop the run and retain workspace {cwd}"
                ) from error
            raise
        os.close(control_read)
        os.close(report_write)
        self.control, self.report = control_write, report_read

    def _unconfirmed(self) -> ProcessCleanupError:
        return ProcessCleanupError(
            f"Gym process cleanup unconfirmed for owner {self.process.pid}; "
            f"stop the run and retain workspace {self.cwd}"
        )

    def wait(self, timeout: float) -> int:
        code = self.process.wait(timeout=timeout)
        if self.report is None:
            return code
        if self.result is None:
            try:
                raw = os.read(self.report, 1025)
                result = json.loads(raw)
                if code != 0 or len(raw) > 1024 or not isinstance(result, dict):
                    raise ValueError("invalid supervisor acknowledgement")
                if set(result) not in ({"returncode"}, {"errno"}):
                    raise ValueError("invalid supervisor result")
                if any(type(value) is not int for value in result.values()):
                    raise ValueError("invalid supervisor result type")
                self.result = result
            except (OSError, ValueError) as error:
                raise self._unconfirmed() from error
        if "errno" in self.result:
            number = self.result["errno"]
            raise OSError(number, os.strerror(number), self.argv[0])
        return self.result["returncode"]

    def terminate(self) -> int:
        self.cleanup_started = True
        try:
            if self.control is not None:
                os.close(self.control)
                self.control = None
            elif self.report is None and self.process.poll() is None:
                self.process.kill()
            return self.wait(PROCESS_REAP_TIMEOUT_SECS)
        except (subprocess.TimeoutExpired, OSError, KeyboardInterrupt, SystemExit) as error:
            # Do not kill the Linux reaper: it must retain descendants until
            # they actually exit. Its control endpoint is already closed.
            raise self._unconfirmed() from error

    def close(self) -> None:
        try:
            if self.result is None and not self.cleanup_started:
                self.terminate()
        finally:
            for name in ("control", "report"):
                descriptor = getattr(self, name)
                if descriptor is not None:
                    os.close(descriptor)
                    setattr(self, name, None)


def validate_timeout(timeout: int) -> int:
    """Reject invalid deadlines before a caller creates processes or workspaces."""
    if isinstance(timeout, bool) or not isinstance(timeout, int) or timeout < 1:
        raise ValueError("timeout must be an integer of at least 1")
    try:
        finite = math.isfinite(timeout)
    except OverflowError:
        finite = False
    if not finite:
        raise ValueError("timeout is too large to represent as a finite deadline")
    return timeout


def run_bounded(
    argv: list[str], cwd: Path, env: dict[str, str], timeout: int, *, stdout_limit: int | None = None,
) -> subprocess.CompletedProcess[bytes]:
    """Capture diagnostic tails, or complete stdout up to a caller's limit.

    Limited stdout retains one overflow byte and terminates the child on
    overflow. Callers must reject that result instead of accepting a prefix.
    """
    deadline = time.monotonic() + validate_timeout(timeout)
    tails = [bytearray(), bytearray()]
    with selectors.DefaultSelector() as selector:
        owned = _OwnedProcess(argv, cwd, env)
        process = owned.process
        try:
            for stream, tail in zip((process.stdout, process.stderr), tails):
                os.set_blocking(stream.fileno(), False)
                selector.register(stream, selectors.EVENT_READ, tail)
            while selector.get_map():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise subprocess.TimeoutExpired(argv, timeout)
                for key, _ in selector.select(min(remaining, 60.0)):
                    try:
                        chunk = os.read(key.fd, 64 * 1024)
                    except (BlockingIOError, InterruptedError):
                        continue
                    if chunk:
                        if stdout_limit is not None and key.data is tails[0]:
                            key.data.extend(chunk[:stdout_limit + 1 - len(key.data)])
                            if len(key.data) > stdout_limit:
                                returncode = owned.terminate()
                                return subprocess.CompletedProcess(argv, returncode, bytes(tails[0]), bytes(tails[1]))
                        else:
                            key.data.extend(chunk)
                            del key.data[:-OUTPUT_TAIL_BYTES]
                    else:
                        selector.unregister(key.fileobj)
                        key.fileobj.close()
            # A child can close both pipes and continue running. Pipe EOF does
            # not remove the deadline on its exit.
            returncode = owned.wait(timeout=max(0, deadline - time.monotonic()))
            return subprocess.CompletedProcess(argv, returncode, bytes(tails[0]), bytes(tails[1]))
        except subprocess.TimeoutExpired:
            raise subprocess.TimeoutExpired(
                argv, timeout, output=bytes(tails[0]), stderr=bytes(tails[1])
            ) from None
        finally:
            process.stdout.close()
            process.stderr.close()
            owned.close()
