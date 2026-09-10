"""Native macOS Gym process ownership, including detached and orphaned descendants."""
import concurrent.futures
import contextlib
import errno
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

from scripts.gym import process_capture as capture
from scripts.gym._macos_supervisor import Processes

ROOT = Path(__file__).resolve().parents[2]

# Each process waits for explicit fixture readiness before the root may finish.
NODE = r'''
import os,signal,subprocess,sys,time
from pathlib import Path
root=Path(sys.argv[1]); topology=sys.argv[2]; outcome=sys.argv[3]
(root/'root.pid').write_text(str(os.getpid())+'\n')
leaf="""import os,signal,sys; from pathlib import Path
Path(sys.argv[1]).write_text(str(os.getpid())+'\\n')
signal.pause()
"""
if topology=='double-fork':
    child=os.fork()
    if child==0:
        os.setsid()
        if os.fork(): os._exit(0)
        exec(leaf.replace("sys.argv[1]",repr(str(root/'leaf.pid'))))
    os.waitpid(child,0)
else:
    subprocess.Popen([sys.executable,'-c',leaf,str(root/'leaf.pid')],start_new_session=topology=='detached')
while not (root/'go').exists(): time.sleep(.005)
os.write(1,b'OUT');os.write(2,b'ERR')
if outcome=='timeout': signal.pause()
elif outcome=='overflow': os.write(1,b'x'*10000);signal.pause()
elif outcome=='failure':sys.exit(7)
elif outcome=='signal':os.kill(os.getpid(),signal.SIGTERM)
'''


def live_identity(processes, path):
    try:
        text = path.read_text()
    except FileNotFoundError:
        return None
    if not text.endswith('\n'):
        return None
    pid = int(text)
    return pid, processes.identity(pid)


def gone(processes, owned):
    pid, identity = owned
    try:
        current = processes.identity(pid)
    except OSError as error:
        if error.errno == errno.ESRCH:
            return True
        raise
    return current.unique != identity.unique


@unittest.skipUnless(sys.platform == 'darwin', 'native macOS launchd ownership')
class MacGymOwnership(unittest.TestCase):
    def test_descendants_settle_before_exit_timeout_and_overflow_return(self):
        processes = Processes()
        with subprocess.Popen(['/bin/sleep', '120']) as sibling:
            try:
                for topology in ('ordinary', 'detached', 'double-fork'):
                    for outcome in ('success', 'failure', 'signal', 'timeout', 'overflow'):
                        with self.subTest(topology=topology, outcome=outcome), tempfile.TemporaryDirectory() as directory:
                            root = Path(directory)
                            argv = [sys.executable, '-c', NODE, str(root), topology, outcome]
                            owned = []
                            stop = False
                            def clock():
                                return 2.0 if stop or (outcome == 'timeout' and (root/'go').exists()) else 0.0
                            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool, \
                                 mock.patch.object(capture, 'time', mock.Mock(monotonic=clock)):
                                result = pool.submit(capture.run_bounded, argv, ROOT, dict(os.environ), 1,
                                                     stdout_limit=8 if outcome == 'overflow' else None)
                                try:
                                    deadline = time.monotonic()+15
                                    while len(owned) != 2:
                                        owned = [item for name in ('root.pid', 'leaf.pid')
                                                 if (item := live_identity(processes, root/name)) is not None]
                                        if result.done(): result.result()
                                        self.assertLess(time.monotonic(), deadline, 'tree readiness stalled')
                                        time.sleep(.005)
                                    if topology != 'ordinary':
                                        self.assertNotEqual(os.getsid(owned[1][0]), os.getsid(owned[0][0]))
                                    # Wrong versions cannot signal a recycled/exec'd identity.
                                    wrong = type(owned[1][1]).from_buffer_copy(owned[1][1])
                                    wrong.version ^= 0x40000000
                                    processes.signal(owned[1][0], wrong, signal.SIGKILL)
                                    self.assertFalse(gone(processes, owned[1]))
                                    (root/'go').touch()
                                    if outcome == 'timeout':
                                        with self.assertRaises(subprocess.TimeoutExpired): result.result(timeout=15)
                                    else:
                                        report = result.result(timeout=15)
                                        if outcome == 'overflow': self.assertEqual(len(report.stdout), 9)
                                        else:
                                            self.assertEqual(report.returncode, {'success':0, 'failure':7, 'signal':-signal.SIGTERM}[outcome])
                                            self.assertEqual((report.stdout, report.stderr), (b'OUT', b'ERR'))
                                    self.assertTrue(all(gone(processes, identity) for identity in owned), 'capture returned with live descendants')
                                    self.assertIsNone(sibling.poll(), 'cleanup touched unrelated process')
                                finally:
                                    stop = True
                                    (root/'go').touch()
                                    for pid, identity in owned:
                                        with contextlib.suppress(OSError): processes.signal(pid, identity, signal.SIGKILL)
                                    with contextlib.suppress(Exception): result.result(timeout=15)
            finally:
                sibling.kill()
                sibling.wait()

    def test_caller_death_settles_detached_tree(self):
        processes = Processes()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            argv = [sys.executable, '-c', NODE, str(root), 'double-fork', 'timeout']
            driver = 'from scripts.gym.process_capture import run_bounded; from pathlib import Path; import os; run_bounded('+repr(argv)+',Path.cwd(),dict(os.environ),60)'
            caller = subprocess.Popen([sys.executable, '-c', driver], cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            owned = []
            try:
                deadline = time.monotonic()+15
                while len(owned) != 2:
                    owned = [item for name in ('root.pid', 'leaf.pid')
                             if (item := live_identity(processes, root/name)) is not None]
                    self.assertIsNone(caller.poll(), 'capture caller exited before readiness')
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(.005)
                caller.kill();caller.wait(timeout=15)
                while not all(gone(processes, identity) for identity in owned):
                    self.assertLess(time.monotonic(), deadline, 'caller death left descendants live')
                    time.sleep(.005)
            finally:
                if caller.poll() is None: caller.kill()
                caller.communicate(timeout=15)
                for pid, identity in owned:
                    with contextlib.suppress(OSError): processes.signal(pid, identity, signal.SIGKILL)
