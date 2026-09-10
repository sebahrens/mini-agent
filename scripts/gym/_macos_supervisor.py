"""Native Gym ownership via a disposable launchd resource coalition.

The launchd job owns every fork/exec descendant, including new sessions and
reparented double forks. This is lifecycle management, not hostile-code isolation.
"""
from __future__ import annotations

import array
import ctypes
import errno
import json
import os
from pathlib import Path
import select
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import uuid


class Identity(ctypes.Structure):
    _fields_ = [("uuid", ctypes.c_ubyte * 16), ("unique", ctypes.c_uint64),
                ("parent", ctypes.c_uint64), ("version", ctypes.c_int32),
                ("parent_version", ctypes.c_int32), ("reserved", ctypes.c_uint64 * 2)]


class Processes:
    def __init__(self):
        self.lib = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
        self.lib.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64,
                                         ctypes.c_void_p, ctypes.c_int]
        self.lib.proc_listallpids.argtypes = [ctypes.c_void_p, ctypes.c_int]
        self.lib.proc_signal_with_audittoken.argtypes = [ctypes.c_void_p, ctypes.c_int]

    def info(self, pid, flavor, buffer):
        count = self.lib.proc_pidinfo(pid, flavor, 0, ctypes.byref(buffer), ctypes.sizeof(buffer))
        if count != ctypes.sizeof(buffer):
            number = ctypes.get_errno() if count == 0 else errno.EIO
            raise OSError(number, "cannot observe Gym process identity")
        return buffer

    def identity(self, pid):
        return self.info(pid, 17, Identity())

    def coalition(self, pid):
        return self.info(pid, 20, (ctypes.c_uint64 * 5)())[0]

    def members(self, coalition):
        # A full buffer is ambiguous: grow and repeat, never accept truncation.
        capacity = 1024
        while True:
            pids = (ctypes.c_int * capacity)()
            count = self.lib.proc_listallpids(pids, ctypes.sizeof(pids))
            if count < 0:
                raise OSError(ctypes.get_errno(), "cannot enumerate Gym coalition")
            if count < capacity:
                break
            capacity *= 2
        members = []
        for pid in pids[:count]:
            if pid <= 0:
                continue
            try:
                before = self.identity(pid)
                if self.coalition(pid) != coalition:
                    continue
                after = self.identity(pid)
                if (before.unique, before.version) == (after.unique, after.version):
                    members.append((pid, after))
                else:
                    # Retry the complete observation after exec or PID reuse.
                    raise OSError(errno.EAGAIN, "Gym process changed during observation")
            except OSError as error:
                if error.errno != errno.ESRCH:
                    raise
        return members

    def signal(self, pid, identity, number):
        # The kernel matches pid + idversion atomically with signal delivery.
        # A recycled PID or intervening exec cannot receive this signal.
        token = (ctypes.c_uint32 * 8)()
        token[5], token[7] = pid, identity.version
        error = self.lib.proc_signal_with_audittoken(token, number)
        if error and error != errno.ESRCH:
            raise OSError(error, "cannot signal owned Gym process")


def receive_exact(peer, size):
    result = bytearray()
    while len(result) < size:
        chunk = peer.recv(size - len(result))
        if not chunk:
            raise EOFError("Gym launch handshake closed")
        result.extend(chunk)
    return bytes(result)


def reap_coalition(processes, coalition, root):
    while True:
        root.poll()  # Reap our direct child before acknowledging cleanup.
        try:
            members = [(pid, identity) for pid, identity in processes.members(coalition)
                       if pid != os.getpid()]
            if not members and root.returncode is not None:
                return
            for pid, identity in members:
                processes.signal(pid, identity, signal.SIGKILL)
        except OSError:
            # Retain the job and its ownership if observation/signalling fails.
            # The caller's bounded wait fails closed and preserves workspaces.
            pass
        time.sleep(0.01)


def job(path):
    with socket.socket(socket.AF_UNIX) as peer:
        peer.settimeout(10)
        peer.connect(path)
        data, ancillary, flags, _ = peer.recvmsg(1, socket.CMSG_SPACE(4 * array.array("i").itemsize))
        descriptors = array.array("i")
        for level, kind, payload in ancillary:
            if level == socket.SOL_SOCKET and kind == socket.SCM_RIGHTS:
                descriptors.frombytes(payload)
        if data != b"F" or flags & socket.MSG_CTRUNC or len(descriptors) != 4:
            raise RuntimeError("invalid Gym descriptor handoff")
        control, report, stdout, stderr = descriptors
        size, = struct.unpack("!I", receive_exact(peer, 4))
        if size > 4 * 1024 * 1024:
            raise RuntimeError("Gym launch configuration exceeds limit")
        config = json.loads(receive_exact(peer, size))
        os.dup2(stdout, 1)
        os.dup2(stderr, 2)
        os.close(stdout)
        os.close(stderr)
        stopping = False

        def stop(_number, _frame):
            nonlocal stopping
            stopping = True

        for number in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
            signal.signal(number, stop)
        try:
            processes = Processes()
            coalition = processes.coalition(os.getpid())
            if not coalition or coalition == config["parent_coalition"]:
                raise OSError(errno.EPERM, "launchd did not establish independent Gym ownership")
            if {pid for pid, _ in processes.members(coalition)} != {os.getpid()}:
                raise OSError(errno.EPERM, "Gym coalition is not exclusive")
            root = subprocess.Popen(config["argv"], cwd=config["cwd"], env=config["env"], close_fds=True)
        except OSError as error:
            # No target was created: acknowledge that fact and release the job.
            # An observation failure must not masquerade as a leaked process tree.
            os.write(2, f"Gym command startup failed: {error}\n".encode())
            result = {"errno": error.errno}
        else:
            try:
                while root.poll() is None and not stopping:
                    if select.select([control], [], [], 0.02)[0]:
                        break
            finally:
                reap_coalition(processes, coalition, root)
            result = {"returncode": root.returncode}
        try:
            os.write(report, json.dumps(result).encode())
        finally:
            # Caller death closes its report endpoint, but confirmed tree
            # cleanup still authorizes the broker to remove this launchd job.
            peer.sendall(b"done")
        os.close(control)
        os.close(report)


def broker(control, report, argv):
    label = "org.mini-agent.gym." + uuid.uuid4().hex
    directory = tempfile.mkdtemp(prefix="ma-gym-", dir="/tmp")
    path = str(Path(directory) / "owner.sock")
    submitted = False
    handed_off = False
    confirmed = False
    try:
        with socket.socket(socket.AF_UNIX) as listener:
            listener.bind(path)
            listener.listen(1)
            listener.settimeout(10)
            command = [sys.executable, "-I", "-S", str(Path(__file__).resolve()), "--job", path]
            subprocess.run(["/bin/launchctl", "submit", "-l", label, "--", *command],
                           check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
            submitted = True
            subprocess.run(["/bin/launchctl", "start", label], check=True,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
            with listener.accept()[0] as peer:
                configuration = json.dumps({"argv": argv, "cwd": os.getcwd(), "env": dict(os.environ),
                                            "parent_coalition": Processes().coalition(os.getpid())}).encode()
                handed_off = True
                peer.sendmsg([b"F"], [(socket.SOL_SOCKET, socket.SCM_RIGHTS,
                                      array.array("i", [control, report, 1, 2]))])
                peer.sendall(struct.pack("!I", len(configuration)) + configuration)
                # The job keeps the control endpoint and coalition alive if the
                # trainer dies; never kill that owner to meet a caller deadline.
                confirmed = receive_exact(peer, 4) == b"done"
    finally:
        if not handed_off or confirmed:
            if submitted:
                subprocess.run(["/bin/launchctl", "remove", label], check=True,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
            Path(path).unlink(missing_ok=True)
            Path(directory).rmdir()
    if not confirmed:
        raise RuntimeError("Gym coalition cleanup not acknowledged")


if __name__ == "__main__":
    if sys.argv[1] == "--job":
        job(sys.argv[2])
    else:
        broker(int(sys.argv[1]), int(sys.argv[2]), sys.argv[3:])
