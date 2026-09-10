"""Owned Linux process trees for Gym lifecycle integration tests.

The test controls a separate capture caller and native commands over local packet
sockets. Every command stays gated until its pidfd has been retained. Closing the
fixture releases peers and rescues only retained identities, never raw PID files.
"""
from __future__ import annotations

import contextlib
import json
import math
import os
import select
import signal
import socket
import struct
import subprocess
import sys
import time
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[2]
HELPER = Path(__file__).resolve()
GUARD = 10


def require(condition: object, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def receive(peer: socket.socket) -> dict:
    packet = peer.recv(8192)
    require(bool(packet), "fixture peer closed before its message")
    return json.loads(packet)


def require_reaped(descriptor: int, role: str) -> None:
    # pidfd POLLIN also includes zombies. POLLHUP proves reaping, while the
    # retained descriptor prevents a recycled PID from changing the identity.
    poll = select.poll()
    poll.register(descriptor, select.POLLIN)
    events = poll.poll(0)
    flags = events[0][1] if events else 0
    require(not flags & (select.POLLERR | select.POLLNVAL), f"invalid pidfd for {role}")
    require(bool(flags & select.POLLHUP), f"{role} was not reaped (pidfd events={flags})")


class GymTree:
    def __init__(self, root: Path, topology: str, outcome: str) -> None:
        self.topology, self.outcome = topology, outcome
        self.path = root / "control.sock"
        self.argv = [sys.executable, str(HELPER), "node", str(self.path), topology, outcome, "root"]
        self.listener = None
        self.caller = None
        self.connections = []
        self.peers, self.pids, self.pidfds = {}, {}, {}

    def __enter__(self):
        return self

    def __exit__(self, kind, error, traceback):
        try:
            self.close()
        except BaseException as cleanup_error:
            if error is None:
                raise
            error.add_note(f"Gym fixture cleanup also failed: {cleanup_error!r}")

    def _track(self, role: str, pid: int) -> None:
        self.pidfds[role] = os.pidfd_open(pid)
        self.pids[role] = pid
        require(not select.select([self.pidfds[role]], [], [], 0)[0], f"{role} exited before readiness")

    def start(self) -> None:
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
        self.listener.settimeout(GUARD)
        self.listener.bind(str(self.path))
        self.listener.listen(8)
        self.caller = subprocess.Popen(
            [sys.executable, "-m", "scripts.tests.gym_process_fixture", "driver",
             str(self.path), self.topology, self.outcome],
            cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        roles = {"caller", "owner", "root", "leaf"}
        if self.topology == "double-fork":
            roles.add("middle")
        while set(self.peers) != roles:
            peer, _ = self.listener.accept()
            self.connections.append(peer)
            peer.settimeout(GUARD)
            message = receive(peer)
            role, pid = message["role"], message["pid"]
            require(role in roles and role not in self.peers, f"unexpected fixture role {role}")
            credential_pid, _, _ = struct.unpack("3i", peer.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            require(credential_pid == (self.caller.pid if role == "owner" else pid), "readiness peer identity mismatch")
            self.peers[role] = peer
            self._track(role, pid)
        require(not select.select(list(self.pidfds.values()), [], [], 0)[0], "tree exited before release")
        leaf_pid = self.pids["leaf"]
        expected_session = leaf_pid if self.topology != "ordinary" else os.getsid(self.pids["root"])
        require(os.getsid(leaf_pid) == expected_session, "fixture did not establish the requested session topology")
        if self.topology == "double-fork":
            self.peers["middle"].send(b"exit")
            self.wait_exited(["middle"])
            status = Path(f"/proc/{leaf_pid}/status").read_text()
            parent = next(int(line.split()[1]) for line in status.splitlines() if line.startswith("PPid:"))
            require(parent == self.pids["owner"], "detached leaf was not adopted by the subreaper")

    def run(self) -> dict | None:
        self.peers["owner"].send(b"run")
        self.peers["root"].send(b"run")
        if self.outcome in ("timeout", "parent-death"):
            require(receive(self.peers["caller"]) == {"event": "captured"}, "caller did not capture diagnostics")
            require(not select.select([self.pidfds[role] for role in ("root", "leaf")], [], [], 0)[0],
                    "command exited before the lifecycle trigger")
            if self.outcome == "parent-death":
                self.caller.kill()
                require(self.caller.wait(timeout=GUARD) == -signal.SIGKILL, "caller did not die abruptly")
                # Parent-death cleanup is asynchronous. Wait on the retained
                # owner identity, then inspect command identities before rescue.
                self.wait_exited(["owner"])
                self.assert_commands_exited()
                return None
            self.peers["caller"].send(b"expire")
        result = receive(self.peers["caller"])
        require(self.caller.wait(timeout=GUARD) == 0, "capture driver failed")
        require(select.select([self.pidfds["owner"]], [], [], 0)[0], "capture returned without owner exit")
        self.assert_commands_exited()
        return result

    def assert_commands_exited(self) -> None:
        for role in ("root", "leaf", "middle"):
            if role in self.pidfds:
                require_reaped(self.pidfds[role], role)

    def wait_exited(self, roles, *, reaped=False) -> None:
        pending = {self.pidfds[role]: role for role in roles}
        poll = select.poll()
        for descriptor in pending:
            # HUP is reported even with an empty interest mask. Avoid waking
            # repeatedly on zombie POLLIN while waiting for actual reaping.
            poll.register(descriptor, 0 if reaped else select.POLLIN)
        deadline = time.monotonic() + GUARD
        while pending:
            ready = poll.poll(max(0, math.ceil((deadline - time.monotonic()) * 1000)))
            require(bool(ready), f"fixture processes did not {'reap' if reaped else 'exit'}: {list(pending.values())}")
            for descriptor, flags in ready:
                require(not flags & (select.POLLERR | select.POLLNVAL), "invalid fixture identity during join")
                require(flags & (select.POLLHUP if reaped else select.POLLIN | select.POLLHUP), "unexpected pidfd join event")
                poll.unregister(descriptor)
                del pending[descriptor]

    def close(self) -> None:
        # Acceptance occurs in run(), before any gate release or rescue here.
        # Register every cleanup step so one failure cannot skip the others.
        def stop(descriptor):
            try:
                signal.pidfd_send_signal(descriptor, signal.SIGKILL)
            except ProcessLookupError:
                pass

        def settle_caller():
            try:
                self.caller.communicate(timeout=GUARD)
            except subprocess.TimeoutExpired:
                self.caller.kill()
                self.caller.communicate(timeout=GUARD)
                raise

        def settle_owner():
            try:
                self.wait_exited(["owner"])
            except AssertionError:
                # A broken variant may strand its reaper after the fixture
                # commands have been stopped. Rescue only that retained
                # identity, and keep the failed join visible to the test.
                stop(self.pidfds["owner"])
                self.wait_exited(["owner"])
                raise

        with contextlib.ExitStack() as cleanup:
            for descriptor in self.pidfds.values():
                cleanup.callback(os.close, descriptor)
            if self.caller is not None:
                cleanup.callback(self.caller.stdout.close)
                cleanup.callback(self.caller.stderr.close)
            if self.pidfds:
                cleanup.callback(self.wait_exited, self.pidfds, reaped=True)
            if "owner" in self.pidfds:
                cleanup.callback(settle_owner)
            if self.caller is not None:
                cleanup.callback(settle_caller)
            for role, descriptor in self.pidfds.items():
                if role not in ("caller", "owner"):
                    cleanup.callback(stop, descriptor)
            if self.listener is not None:
                cleanup.callback(self.listener.close)
            for peer in self.connections:
                cleanup.callback(peer.close)


@contextlib.contextmanager
def held_sibling():
    sibling = subprocess.Popen(
        [sys.executable, "-c", "import os; os.write(1,b'ready'); os.read(0,1)"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
    )
    try:
        require(select.select([sibling.stdout], [], [], GUARD)[0], "unrelated sibling did not start")
        require(os.read(sibling.stdout.fileno(), 5) == b"ready", "invalid sibling readiness")
        yield sibling
    finally:
        try:
            sibling.communicate(input=b"stop", timeout=GUARD)
        except subprocess.TimeoutExpired:
            sibling.kill()
            sibling.communicate(timeout=GUARD)
        finally:
            sibling.stdin.close()
            sibling.stdout.close()


def connect(path: str, role: str, pid: int) -> socket.socket:
    peer = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    peer.connect(path)
    peer.send(json.dumps({"role": role, "pid": pid}).encode())
    return peer


def node(path: str, topology: str, outcome: str, role: str) -> None:
    if role in ("root", "middle"):
        child = "middle" if role == "root" and topology == "double-fork" else "leaf"
        subprocess.Popen(
            [sys.executable, str(HELPER), "node", path, topology, outcome, child],
            start_new_session=role == "middle" or topology == "detached",
        )
    with connect(path, role, os.getpid()) as peer:
        action = peer.recv(32)
        if role != "root" or action != b"run":
            return
        os.write(1, b"OUT")
        os.write(2, b"ERR")
        if outcome in ("success", "failure"):
            sys.exit(0 if outcome == "success" else 7)
        if outcome == "signal":
            os.kill(os.getpid(), signal.SIGTERM)
        if outcome == "overflow":
            os.write(1, b"x" * 65536)
        peer.recv(32)


def driver(path: str, topology: str, outcome: str) -> None:
    from scripts.gym import process_capture as capture

    now = 100.0
    capture.time = SimpleNamespace(monotonic=lambda: now)
    real_spawn = subprocess.Popen
    real_selector = capture.selectors.DefaultSelector
    with connect(path, "caller", os.getpid()) as controller:
        def spawn(*args, **kwargs):
            owner = real_spawn(*args, **kwargs)
            with connect(path, "owner", owner.pid) as peer:
                require(peer.recv(32) == b"run", "fixture abandoned owner startup")
            return owner

        def selector():
            instance = real_selector()
            real_select = instance.select

            def select_events(timeout):
                nonlocal now
                tails = [key.data for key in instance.get_map().values()]
                if outcome in ("timeout", "parent-death") and tails == [b"OUT", b"ERR"] and now == 100.0:
                    controller.send(json.dumps({"event": "captured"}).encode())
                    require(controller.recv(32) == b"expire", "fixture abandoned capture")
                    now = 101.0
                    return []
                return real_select(timeout)

            instance.select = select_events
            return instance

        capture.subprocess.Popen = spawn
        capture.selectors.DefaultSelector = selector
        argv = [sys.executable, str(HELPER), "node", path, topology, outcome, "root"]
        try:
            result = capture.run_bounded(argv, Path(path).parent, dict(os.environ), 1,
                                         stdout_limit=16 if outcome == "overflow" else None)
            message = {"args": result.args, "returncode": result.returncode,
                       "stdout": result.stdout.decode(), "stderr": result.stderr.decode()}
        except subprocess.TimeoutExpired as error:
            message = {"args": error.cmd, "timeout": error.timeout,
                       "stdout": error.output.decode(), "stderr": error.stderr.decode()}
        controller.send(json.dumps(message).encode())


if __name__ == "__main__":
    if sys.argv[1] == "driver":
        driver(*sys.argv[2:])
    else:
        node(*sys.argv[2:])
