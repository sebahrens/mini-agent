"""Bounded diagnostic output capture shared by the Gym trainer and task miner.

Results normally contain diagnostic byte tails. A stdout limit selects complete
capture with an overflow sentinel for callers such as the miner's Git blob reader.
"""

from __future__ import annotations

import contextlib
import json
import math
import os
import selectors
import socket
import subprocess
import sys
import time
from pathlib import Path

OUTPUT_TAIL_BYTES = 2000
PROCESS_REAP_TIMEOUT_SECS = 5


class ProcessCleanupError(RuntimeError):
    """Cleanup was not confirmed; callers must stop and preserve workspaces."""


ACKNOWLEDGEMENT_LIMIT = 1024
_monotonic = time.monotonic  # Real clock, independent of caller deadline control.


class AcknowledgementError(ValueError):
    """The supervisor's cleanup acknowledgement is missing or malformed."""


def read_acknowledgement(fd: int, timeout: float) -> bytes:
    """Read one supervisor acknowledgement frame from non-blocking ``fd``.

    The supervisor writes a single JSON object. Partial reads accumulate until
    the buffer holds a complete JSON object or the stream reaches EOF; a frame
    that does not complete within ``timeout`` or exceeds the size limit is
    rejected so the caller fails closed. EOF with no bytes is returned as an
    empty frame for ``parse_acknowledgement`` to reject distinctly.
    """
    deadline = _monotonic() + timeout
    raw = bytearray()
    with selectors.DefaultSelector() as selector:
        selector.register(fd, selectors.EVENT_READ)
        while True:
            try:
                chunk = os.read(fd, ACKNOWLEDGEMENT_LIMIT + 1 - len(raw))
            except (BlockingIOError, InterruptedError):
                remaining = deadline - _monotonic()
                if remaining <= 0 or not selector.select(remaining):
                    raise AcknowledgementError(
                        "supervisor acknowledgement incomplete" if raw
                        else "supervisor acknowledgement not received"
                    ) from None
                continue
            if not chunk:
                return bytes(raw)
            raw.extend(chunk)
            if len(raw) > ACKNOWLEDGEMENT_LIMIT:
                raise AcknowledgementError("supervisor acknowledgement exceeds limit")
            if _complete_object(raw):
                return bytes(raw)


def _complete_object(raw: bytes) -> bool:
    # A JSON object cannot be extended past its closing brace, so a buffer that
    # already parses as one is a complete frame even before EOF arrives (a
    # caller can still hold a write endpoint after an interrupted handoff).
    try:
        return isinstance(json.loads(raw), dict)
    except ValueError:
        return False


def parse_acknowledgement(raw: bytes) -> dict[str, int]:
    """Validate one complete frame returned by ``read_acknowledgement``."""
    if not raw:
        raise AcknowledgementError("supervisor closed without acknowledgement")
    try:
        result = json.loads(raw)
    except ValueError as error:
        raise AcknowledgementError("malformed supervisor acknowledgement") from error
    if not isinstance(result, dict) or set(result) not in ({"returncode"}, {"errno"}):
        raise AcknowledgementError("invalid supervisor result")
    if any(type(value) is not int for value in result.values()):
        raise AcknowledgementError("invalid supervisor result type")
    return result


class _OwnedProcess:
    def __init__(self, argv: list[str], cwd: Path) -> None:
        self.argv = argv
        self.cwd = cwd
        self.process: subprocess.Popen | None = None
        self.control: socket.socket | None = None
        self.report: socket.socket | None = None
        self.endpoints: list[tuple[socket.socket, socket.socket]] = []
        self.result: dict[str, int] | None = None
        self.cleanup_started = False
        self.stderr_tail = bytearray()

    def start(self, env: dict[str, str]) -> None:
        # The caller already owns this object and will close it even when
        # launch or the parent-side endpoint handoff is interrupted.
        try:
            if sys.platform not in {"linux", "darwin"}:
                self.process = subprocess.Popen(
                    self.argv, cwd=self.cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                )
                return
            self.endpoints.append(socket.socketpair())
            control_read, self.control = self.endpoints[-1]
            self.endpoints.append(socket.socketpair())
            self.report, report_write = self.endpoints[-1]
            # Complete fallible endpoint setup before a command can start.
            os.set_blocking(self.report.fileno(), False)
            supervisor_name = "_macos_supervisor.py" if sys.platform == "darwin" else "_process_supervisor.py"
            supervisor = str(Path(__file__).with_name(supervisor_name).resolve())
            self.process = subprocess.Popen(
                [sys.executable, "-I", "-S", supervisor,
                 str(control_read.fileno()), str(report_write.fileno()), *self.argv],
                cwd=self.cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                pass_fds=(control_read.fileno(), report_write.fileno()),
            )
            # Socket objects invalidate their descriptor on close, so cleanup
            # can safely close them again after an interrupted handoff.
            control_read.close()
            report_write.close()
        except (KeyboardInterrupt, SystemExit) as error:
            if self.process is None:
                # Popen can be interrupted after creating the supervisor.
                # Endpoint cleanup requests cancellation, but without the
                # returned handle we cannot confirm that it completed.
                raise ProcessCleanupError(
                    f"Gym owner startup interrupted; stop the run and retain workspace {self.cwd}"
                ) from error
            raise

    def _unconfirmed(self, reason: str = "") -> ProcessCleanupError:
        owner = f"for owner {self.process.pid}" if self.process is not None else "during startup"
        return ProcessCleanupError(
            f"Gym process cleanup unconfirmed {owner}"
            + (f" ({reason})" if reason else "") + "; "
            f"stop the run and retain workspace {self.cwd}; "
            f"supervisor stderr: {bytes(self.stderr_tail).decode(errors='replace')}"
        )

    def _drain_stderr(self) -> None:
        # Collect late supervisor diagnostics (an overflow stops the capture
        # loop first) without blocking on a descendant that holds the pipe.
        stream = self.process.stderr
        if stream is None or stream.closed:
            return
        with contextlib.suppress(OSError, ValueError):
            os.set_blocking(stream.fileno(), False)
            for _ in range(64):
                chunk = os.read(stream.fileno(), 64 * 1024)
                if not chunk:
                    break
                self.stderr_tail.extend(chunk)
                del self.stderr_tail[:-OUTPUT_TAIL_BYTES]

    def wait(self, timeout: float) -> int:
        code = self.process.wait(timeout=timeout)
        if self.report is None:
            return code
        if self.result is None:
            try:
                if code != 0:
                    # A failed owner cannot confirm its own teardown, even when
                    # the command tree acknowledged before it failed.
                    raw = b""
                    with contextlib.suppress(OSError, ValueError):
                        raw = read_acknowledgement(self.report.fileno(), 0)
                    raise AcknowledgementError(
                        f"supervisor exited with status {code}"
                        + (" after acknowledgement" if raw else " without acknowledgement")
                    )
                raw = read_acknowledgement(self.report.fileno(), PROCESS_REAP_TIMEOUT_SECS)
                self.result = parse_acknowledgement(raw)
            except (OSError, ValueError) as error:
                self._drain_stderr()
                raise self._unconfirmed(str(error)) from error
        if "errno" in self.result:
            number = self.result["errno"]
            raise OSError(number, os.strerror(number), self.argv[0])
        return self.result["returncode"]

    def terminate(self) -> int:
        self.cleanup_started = True
        try:
            if self.control is not None:
                self.control.close()
            elif self.report is None and self.process.poll() is None:
                self.process.kill()
            return self.wait(PROCESS_REAP_TIMEOUT_SECS)
        except (subprocess.TimeoutExpired, OSError, KeyboardInterrupt, SystemExit) as error:
            # Do not kill the process owner: it must retain descendants until
            # they actually exit. Its control endpoint is already closed.
            raise self._unconfirmed() from error

    def close(self) -> None:
        # Attempt every resource close even if another close or the cleanup
        # acknowledgement fails. Keep the process owner alive on failure.
        try:
            with contextlib.ExitStack() as cleanup:
                for pair in self.endpoints:
                    for endpoint in pair:
                        cleanup.callback(endpoint.close)
                if self.process is not None:
                    cleanup.callback(self.process.stdout.close)
                    cleanup.callback(self.process.stderr.close)
                    if self.result is None and not self.cleanup_started:
                        self.terminate()
        except ProcessCleanupError:
            raise
        except BaseException as error:
            # A finalizer must not turn unconfirmed process cleanup into a
            # recoverable I/O error that allows workspace deletion.
            raise self._unconfirmed() from error


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
        owned = _OwnedProcess(argv, cwd)
        owned.stderr_tail = tails[1]
        try:
            owned.start(env)
            process = owned.process
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
            owned.close()
