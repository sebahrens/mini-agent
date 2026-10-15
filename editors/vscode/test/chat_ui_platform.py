"""Native process/command adapter for the installed Chat UI fixture only."""
import os
from pathlib import Path
import signal
import subprocess
import sys
import time


def fixture_environment(inherited, platform=sys.platform):
    keys = ['HOME', 'USER', 'LOGNAME', 'PATH', 'TMPDIR', 'SHELL', 'LANG']
    if platform == 'linux':
        keys += ['DISPLAY', 'XAUTHORITY', 'DBUS_SESSION_BUS_ADDRESS']
    if platform == 'win32':
        keys += ['SystemRoot', 'SYSTEMROOT', 'WINDIR', 'ComSpec', 'COMSPEC', 'TEMP',
                 'TMP', 'USERPROFILE', 'APPDATA', 'LOCALAPPDATA', 'PATHEXT']
    return {key: inherited[key] for key in keys if key in inherited}


def launch_command(app, platform=sys.platform):
    if platform == 'darwin':
        return [str(app / 'Contents/MacOS/Code')]
    return [str(app / ('Code.exe' if platform == 'win32' else 'code'))]


def install_command(app, arguments, env, platform=sys.platform):
    if platform == 'win32':
        shell = env.get('ComSpec') or env.get('COMSPEC') or str(Path(env.get('SystemRoot') or env['SYSTEMROOT']) / 'System32/cmd.exe')
        # /d disables user AutoRun; /s /c receives one explicitly quoted command.
        parts = [str(app / 'bin/code.cmd'), *arguments]
        if any(any(char in part for char in '"%\r\n!') for part in parts):
            raise ValueError('unsupported shell expansion character in owned fixture path')
        command = ' '.join('"' + part + '"' for part in parts)
        return [shell, '/d', '/s', '/c', '"' + command + '"']
    cli = app / ('Contents/Resources/app/bin/code' if platform == 'darwin' else 'bin/code')
    return [str(cli), *arguments]


def windows_command_line(arguments):
    if len(arguments) == 5 and arguments[1:4] == ['/d', '/s', '/c']:
        return subprocess.list2cmdline(arguments[:4]) + ' ' + arguments[4]
    return subprocess.list2cmdline(arguments)


class NativeProcesses:
    def __init__(self):
        self.job = _WindowsJob() if sys.platform == 'win32' else None
        self.groups = []

    def start(self, arguments, env, stdout):
        if self.job:
            return self.job.start(arguments, env, stdout)
        child = subprocess.Popen(arguments, env=env, stdout=stdout,
                                 stderr=subprocess.STDOUT, start_new_session=True)
        self.groups.append(child.pid)
        return child

    def stop(self, process, force=False):
        if self.job:
            self.job.terminate()
        else:
            os.killpg(process.pid, signal.SIGKILL if force else signal.SIGTERM)

    def close(self):
        if self.job:
            return self.job.retire()
        def exists(group):
            try:
                os.killpg(group, 0)
                return True
            except ProcessLookupError:
                return False
        forced = []
        for group in self.groups:
            deadline = time.monotonic() + 5
            while exists(group) and time.monotonic() < deadline:
                time.sleep(.01)
            if exists(group):
                forced.append(group)
                os.killpg(group, signal.SIGKILL)
                deadline = time.monotonic() + 5
                while exists(group) and time.monotonic() < deadline:
                    time.sleep(.01)
            if exists(group):
                raise RuntimeError('owned UI process-group descendants did not retire')
        return {'backend':'unix-process-groups', 'process_groups':self.groups,
                'members_remaining':0, 'forced_groups':forced,
                'forced_descendants':bool(forced)}


class _WindowsJob:
    """Create suspended, attach to one kill-on-close Job, then resume."""
    def __init__(self):
        import ctypes as c
        from ctypes import wintypes as w
        self.c, self.w = c, w
        self.kernel = c.WinDLL('kernel32', use_last_error=True)
        class BasicLimits(c.Structure):
            _fields_ = [('process_time', c.c_int64), ('job_time', c.c_int64),
                        ('flags', w.DWORD), ('min_working_set', c.c_size_t),
                        ('max_working_set', c.c_size_t), ('active_process_limit', w.DWORD),
                        ('affinity', c.c_size_t), ('priority', w.DWORD), ('scheduling', w.DWORD)]
        class IoCounters(c.Structure):
            _fields_ = [(name, c.c_uint64) for name in ('read_ops', 'write_ops', 'other_ops', 'read_bytes', 'write_bytes', 'other_bytes')]
        class ExtendedLimits(c.Structure):
            _fields_ = [('basic', BasicLimits), ('io', IoCounters),
                        ('process_memory', c.c_size_t), ('job_memory', c.c_size_t),
                        ('peak_process_memory', c.c_size_t), ('peak_job_memory', c.c_size_t)]
        class StartupInfo(c.Structure):
            _fields_ = [('cb', w.DWORD), ('reserved', w.LPWSTR), ('desktop', w.LPWSTR),
                        ('title', w.LPWSTR), ('x', w.DWORD), ('y', w.DWORD),
                        ('xsize', w.DWORD), ('ysize', w.DWORD), ('xchars', w.DWORD),
                        ('ychars', w.DWORD), ('fill', w.DWORD), ('flags', w.DWORD),
                        ('show', w.WORD), ('reserved_size', w.WORD),
                        ('reserved_bytes', c.POINTER(c.c_ubyte)),
                        ('stdin', w.HANDLE), ('stdout', w.HANDLE), ('stderr', w.HANDLE)]
        class ProcessInfo(c.Structure):
            _fields_ = [('process', w.HANDLE), ('thread', w.HANDLE),
                        ('pid', w.DWORD), ('tid', w.DWORD)]
        self.StartupInfo, self.ProcessInfo = StartupInfo, ProcessInfo
        declarations = {
            'CreateJobObjectW': ([c.c_void_p, w.LPCWSTR], w.HANDLE),
            'SetInformationJobObject': ([w.HANDLE, c.c_int, c.c_void_p, w.DWORD], w.BOOL),
            'QueryInformationJobObject': ([w.HANDLE, c.c_int, c.c_void_p, w.DWORD, c.c_void_p], w.BOOL),
            'AssignProcessToJobObject': ([w.HANDLE, w.HANDLE], w.BOOL),
            'TerminateJobObject': ([w.HANDLE, w.UINT], w.BOOL),
            'CloseHandle': ([w.HANDLE], w.BOOL),
            'CreateProcessW': ([w.LPCWSTR, w.LPWSTR, c.c_void_p, c.c_void_p, w.BOOL, w.DWORD, c.c_void_p, w.LPCWSTR, c.POINTER(StartupInfo), c.POINTER(ProcessInfo)], w.BOOL),
            'ResumeThread': ([w.HANDLE], w.DWORD),
            'TerminateProcess': ([w.HANDLE, w.UINT], w.BOOL),
            'WaitForSingleObject': ([w.HANDLE, w.DWORD], w.DWORD),
            'GetExitCodeProcess': ([w.HANDLE, c.POINTER(w.DWORD)], w.BOOL),
            'GetCurrentProcess': ([], w.HANDLE),
            'DuplicateHandle': ([w.HANDLE, w.HANDLE, w.HANDLE, c.POINTER(w.HANDLE), w.DWORD, w.BOOL, w.DWORD], w.BOOL),
        }
        for name, (arguments, result) in declarations.items():
            function = getattr(self.kernel, name)
            function.argtypes, function.restype = arguments, result
        self.handle = self.kernel.CreateJobObjectW(None, None)
        if not self.handle: raise c.WinError(c.get_last_error())
        limits = ExtendedLimits()
        limits.basic.flags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if not self.kernel.SetInformationJobObject(self.handle, 9, c.byref(limits), c.sizeof(limits)):
            error = c.WinError(c.get_last_error())
            self.close()
            raise error
        self.processes = []

    def start(self, arguments, env, stdout):
        import msvcrt
        c, w, kernel = self.c, self.w, self.kernel
        inherited = []
        def duplicate(file):
            handle = w.HANDLE()
            current = kernel.GetCurrentProcess()
            if not kernel.DuplicateHandle(current, msvcrt.get_osfhandle(file.fileno()), current, c.byref(handle), 0, True, 2):
                raise c.WinError(c.get_last_error())
            inherited.append(handle)
            return handle
        process = self.ProcessInfo()
        with open(os.devnull, 'rb') as stdin:
            try:
                info = self.StartupInfo()
                info.cb, info.flags = c.sizeof(info), 0x100
                info.stdin, info.stdout = duplicate(stdin), duplicate(stdout)
                info.stderr = info.stdout
                command = c.create_unicode_buffer(windows_command_line(arguments))
                environment = c.create_unicode_buffer('\0'.join(f'{key}={value}' for key, value in sorted(env.items(), key=lambda item:item[0].upper())) + '\0\0')
                # No application code runs before successful Job assignment.
                flags = 0x4 | 0x200 | 0x400  # suspended, new process group, Unicode env
                if not kernel.CreateProcessW(None, command, None, None, True, flags, environment, None, c.byref(info), c.byref(process)):
                    raise c.WinError(c.get_last_error())
                if not kernel.AssignProcessToJobObject(self.handle, process.process):
                    raise c.WinError(c.get_last_error())
                if kernel.ResumeThread(process.thread) != 1:
                    raise RuntimeError('failed to resume exact suspended UI fixture process')
                child = _WindowsProcess(self, process.process, process.pid, arguments)
                self.processes.append(child)
                return child
            except BaseException:
                if process.process:
                    kernel.TerminateProcess(process.process, 1)
                    kernel.WaitForSingleObject(process.process, 5000)
                    kernel.CloseHandle(process.process)
                raise
            finally:
                if process.thread: kernel.CloseHandle(process.thread)
                for handle in inherited: kernel.CloseHandle(handle)

    def terminate(self):
        if self.handle and not self.kernel.TerminateJobObject(self.handle, 1):
            raise self.c.WinError(self.c.get_last_error())

    def active_processes(self):
        c = self.c
        class Accounting(c.Structure):
            _fields_ = [('user', c.c_int64), ('kernel', c.c_int64),
                        ('period_user', c.c_int64), ('period_kernel', c.c_int64),
                        ('faults', self.w.DWORD), ('total', self.w.DWORD),
                        ('active', self.w.DWORD), ('terminated', self.w.DWORD)]
        info = Accounting()
        if not self.kernel.QueryInformationJobObject(self.handle, 1, c.byref(info), c.sizeof(info), None):
            raise c.WinError(c.get_last_error())
        return info.active

    def retire(self):
        forced = False
        try:
            deadline = time.monotonic() + 5
            while self.active_processes() and time.monotonic() < deadline:
                time.sleep(.01)
            if self.active_processes():
                forced = True
                self.terminate()
                deadline = time.monotonic() + 5
                while self.active_processes() and time.monotonic() < deadline:
                    time.sleep(.01)
            remaining = self.active_processes()
            if remaining:
                raise RuntimeError('owned UI Job descendants did not retire')
            return {'backend':'windows-job', 'active_processes':remaining, 'forced_descendants':forced}
        finally:
            self.close()

    def close(self):
        if self.handle:
            self.kernel.CloseHandle(self.handle)
            self.handle = None
        for process in getattr(self, 'processes', []):
            if process.handle:
                self.kernel.CloseHandle(process.handle)
                process.handle = None


class _WindowsProcess:
    def __init__(self, job, handle, pid, arguments):
        self.job, self.handle, self.pid, self.args = job, handle, pid, arguments
        self.returncode = None

    def poll(self):
        result = self.job.kernel.WaitForSingleObject(self.handle, 0)
        if result == 258: return None
        if result != 0: raise self.job.c.WinError(self.job.c.get_last_error())
        code = self.job.w.DWORD()
        if not self.job.kernel.GetExitCodeProcess(self.handle, self.job.c.byref(code)):
            raise self.job.c.WinError(self.job.c.get_last_error())
        self.returncode = code.value
        return self.returncode

    def wait(self, timeout=None):
        milliseconds = 0xffffffff if timeout is None else min(int(timeout * 1000), 0xfffffffe)
        result = self.job.kernel.WaitForSingleObject(self.handle, milliseconds)
        if result == 258: raise subprocess.TimeoutExpired(self.args, timeout)
        if result != 0: raise self.job.c.WinError(self.job.c.get_last_error())
        return self.poll()
