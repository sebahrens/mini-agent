"""Bounded diagnostic output capture shared by the Gym trainer and task miner.

Results normally contain diagnostic byte tails. A stdout limit selects complete
capture with an overflow sentinel for callers such as the miner's Git blob reader.
"""

from __future__ import annotations

import math
import os
import selectors
import subprocess
import time
from pathlib import Path

OUTPUT_TAIL_BYTES = 2000
PROCESS_REAP_TIMEOUT_SECS = 5


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
        process = subprocess.Popen(argv, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
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
                                process.kill()
                                returncode = process.wait(timeout=PROCESS_REAP_TIMEOUT_SECS)
                                return subprocess.CompletedProcess(argv, returncode, bytes(tails[0]), bytes(tails[1]))
                        else:
                            key.data.extend(chunk)
                            del key.data[:-OUTPUT_TAIL_BYTES]
                    else:
                        selector.unregister(key.fileobj)
                        key.fileobj.close()
            # A child can close both pipes and continue running. Pipe EOF does
            # not remove the deadline on its exit.
            returncode = process.wait(timeout=max(0, deadline - time.monotonic()))
            return subprocess.CompletedProcess(argv, returncode, bytes(tails[0]), bytes(tails[1]))
        except subprocess.TimeoutExpired:
            raise subprocess.TimeoutExpired(
                argv, timeout, output=bytes(tails[0]), stderr=bytes(tails[1])
            ) from None
        finally:
            process.stdout.close()
            process.stderr.close()
            if process.poll() is None:
                process.kill()
                try:
                    process.wait(timeout=PROCESS_REAP_TIMEOUT_SECS)
                except subprocess.TimeoutExpired as error:
                    raise OSError("could not reap Gym subprocess after termination") from error
