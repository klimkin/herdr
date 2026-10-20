"""Native process and host CPU counters in the latency harness clock domain."""

import errno
import os
import signal
import subprocess
import sys
import time
from typing import Mapping, TypedDict


class Process(TypedDict):
    ppid: int
    start: int
    cpu_ns: int
    name: str


class Snapshot(TypedDict):
    ns: int
    processes: dict[int, Process]
    host_cpu_ns: int


def descendants(processes: Mapping[int, Process], pid: int) -> set[int]:
    """Return descendant PIDs, excluding the root and tolerating parent cycles.

    An orphan keeps naming its exited parent's PID, so it looks like a child of
    any later process reusing that PID. A real child never starts before its parent.
    """
    children: dict[int, list[int]] = {}
    for child, process in processes.items():
        children.setdefault(process["ppid"], []).append(child)
    visited = {pid}
    pending = [pid]
    while pending:
        parent = pending.pop()
        parent_start = processes[parent]["start"] if parent in processes else None
        for child in children.get(parent, ()):
            if child not in visited and (parent_start is None or processes[child]["start"] >= parent_start):
                visited.add(child)
                pending.append(child)
    visited.remove(pid)
    return visited


class Resources:
    """Read native counters and signal only the caller's supplied identity.

    Callers retain owned PID/start pairs. A snapshot is a sequential scan,
    timestamped after counter reads, rather than an atomic view of all tasks.
    """

    def __init__(self):
        if sys.platform == "linux":
            self._backend = _LinuxResources()
        elif os.name == "nt":
            self._backend = _WindowsResources()
        else:
            raise NotImplementedError("resource sampling supports Linux and Windows")
        self.metadata = self._backend.metadata

    def clock_ns(self) -> int:
        """Use the same clock origin and integer conversion as the harness."""
        return self._backend.clock_ns()

    def snapshot(self) -> Snapshot:
        return self._backend.snapshot()

    def alive(self, pid: int, start: int) -> bool:
        return self._backend.alive(pid, start)

    def terminate(self, pid: int, start: int) -> bool:
        """Request termination only if the PID still has the supplied creation time."""
        if pid <= 0:
            return False
        return self._backend.terminate(pid, start)

    def kill(self, pid: int, start: int) -> bool:
        if pid <= 0:
            return False
        if sys.platform == "linux":
            return self._backend._signal(pid, start, signal.SIGKILL)
        return self._backend.terminate(pid, start)

    def launch(self, command, env, log, cpus=None) -> subprocess.Popen:
        """Start an owned process with affinity applied before its code runs."""
        return self._backend.launch(command, env, log, cpus)

    def close_launch(self, process: subprocess.Popen) -> None:
        """Close owned launch resources and stop retained descendants."""
        self._backend.close_launch(process)


class _LinuxResources:
    def __init__(self):
        self._launches: dict[subprocess.Popen, int] = {}
        self._ticks_per_second = os.sysconf("SC_CLK_TCK")
        self._pidfd_supported = hasattr(os, "pidfd_open") and hasattr(
            signal, "pidfd_send_signal"
        )
        self.metadata = {
            "backend": "linux_procfs",
            "clock": "CLOCK_MONOTONIC",
            "clock_resolution_ns": max(
                1, round(time.clock_getres(time.CLOCK_MONOTONIC) * 1_000_000_000)
            ),
            "cpu_counter_hz": self._ticks_per_second,
            "cpu_counter_unit_ns": {
                "numerator": 1_000_000_000,
                "denominator": self._ticks_per_second,
            },
            "logical_cpus": os.cpu_count(),
            "process_identity": "pid_and_proc_stat_starttime_ticks",
            "process_name": "proc_stat_comm",
            "host_cpu": "user+nice+system+irq+softirq+steal; guest already included",
            "termination": "pidfd_SIGTERM" if self._pidfd_supported else "unavailable",
            "snapshot_timestamp": "after_counter_reads",
            "snapshot_scope": "visible procfs tasks; sequential non-atomic scan",
            "inaccessible_processes": "omitted",
            "launch_cleanup": "identity_checked_pidfds_for_owned_session_process_group",
            "launch_affinity": "taskset_before_target_exec",
        }

    def clock_ns(self) -> int:
        return time.clock_gettime_ns(time.CLOCK_MONOTONIC)

    def _ticks_ns(self, ticks: int) -> int:
        return ticks * 1_000_000_000 // self._ticks_per_second

    def _process(self, pid: int) -> tuple[Process, str] | None:
        try:
            with open(f"/proc/{pid}/stat", encoding="utf-8", errors="replace") as file:
                stat = file.read()
            opening = stat.index("(")
            closing = stat.rindex(")")
            fields = stat[closing + 2 :].split()
            return {
                "ppid": int(fields[1]),
                "start": int(fields[19]),
                "cpu_ns": self._ticks_ns(int(fields[11]) + int(fields[12])),
                "name": stat[opening + 1 : closing],
            }, fields[0]
        except (OSError, ValueError, IndexError):
            return None

    def snapshot(self) -> Snapshot:
        processes: dict[int, Process] = {}
        with os.scandir("/proc") as entries:
            for entry in entries:
                if entry.name.isdecimal():
                    pid = int(entry.name)
                    process = self._process(pid)
                    if process is not None:
                        processes[pid] = process[0]
        with open("/proc/stat", encoding="ascii") as file:
            aggregate = file.readline().split()
        if not aggregate or aggregate[0] != "cpu" or len(aggregate) < 9:
            raise RuntimeError("/proc/stat lacks the aggregate CPU counters")
        counters = [int(value) for value in aggregate[1:]]
        busy_ticks = sum(counters[index] for index in (0, 1, 2, 5, 6, 7))
        return {
            "ns": self.clock_ns(),
            "processes": processes,
            "host_cpu_ns": self._ticks_ns(busy_ticks),
        }

    def alive(self, pid: int, start: int) -> bool:
        process = self._process(pid)
        return (
            process is not None
            and process[0]["start"] == start
            and process[1] not in ("Z", "X", "x")
        )

    def terminate(self, pid: int, start: int) -> bool:
        return self._signal(pid, start, signal.SIGTERM)

    def _signal(self, pid: int, start: int, requested_signal: int) -> bool:
        if not self._pidfd_supported:
            raise NotImplementedError("identity-safe cleanup requires Linux pidfd support")
        try:
            descriptor = os.pidfd_open(pid, 0)
        except OSError as error:
            if error.errno in (errno.ENOSYS, errno.EINVAL):
                raise NotImplementedError(
                    "identity-safe cleanup requires kernel pidfd support"
                ) from error
            if error.errno in (errno.ESRCH, errno.EPERM, errno.EACCES):
                return False
            raise
        try:
            if not self.alive(pid, start):
                return False
            try:
                signal.pidfd_send_signal(descriptor, requested_signal, None, 0)
                return True
            except (ProcessLookupError, PermissionError):
                return False
        finally:
            os.close(descriptor)

    def launch(self, command, env, log, cpus) -> subprocess.Popen:
        if cpus is not None:
            if not cpus or any(not isinstance(cpu, int) or cpu < 0 for cpu in cpus):
                raise ValueError("CPU affinity requires nonnegative CPU indices")
            command = ["taskset", "-c", ",".join(str(cpu) for cpu in cpus), *command]
        process = subprocess.Popen(
            command,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        identity = self._process(process.pid)
        if identity is None:
            process.kill()
            process.wait()
            raise RuntimeError("launched process exited before identity capture")
        self._launches[process] = identity[0]["start"]
        return process

    def close_launch(self, process: subprocess.Popen) -> None:
        start = self._launches.pop(process, None)
        if start is None:
            return
        root = self._process(process.pid)
        if root is not None and root[0]["start"] != start:
            return
        owned: list[tuple[int, int]] = []
        with os.scandir("/proc") as entries:
            for entry in entries:
                if not entry.name.isdecimal():
                    continue
                pid = int(entry.name)
                try:
                    with open(f"/proc/{pid}/stat", encoding="utf-8", errors="replace") as file:
                        fields = file.read().rsplit(")", 1)[1].split()
                    if int(fields[2]) == process.pid and int(fields[3]) == process.pid:
                        identity = int(fields[19])
                        if identity >= start:
                            owned.append((pid, identity))
                except (OSError, ValueError, IndexError):
                    continue
        for pid, identity in owned:
            self._signal(pid, identity, signal.SIGKILL)


class _WindowsResources:
    def __init__(self):
        import ctypes
        from ctypes import wintypes

        self._ctypes = ctypes
        self._kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        self._jobs = {}

        class ProcessEntry(ctypes.Structure):
            _fields_ = [
                ("dwSize", wintypes.DWORD),
                ("cntUsage", wintypes.DWORD),
                ("th32ProcessID", wintypes.DWORD),
                ("th32DefaultHeapID", ctypes.c_size_t),
                ("th32ModuleID", wintypes.DWORD),
                ("cntThreads", wintypes.DWORD),
                ("th32ParentProcessID", wintypes.DWORD),
                ("pcPriClassBase", wintypes.LONG),
                ("dwFlags", wintypes.DWORD),
                ("szExeFile", wintypes.WCHAR * 260),
            ]

        self._entry_type = ProcessEntry
        self._filetime_type = wintypes.FILETIME

        class BasicLimit(ctypes.Structure):
            _fields_ = [
                ("PerProcessUserTimeLimit", ctypes.c_int64),
                ("PerJobUserTimeLimit", ctypes.c_int64),
                ("LimitFlags", wintypes.DWORD),
                ("MinimumWorkingSetSize", ctypes.c_size_t),
                ("MaximumWorkingSetSize", ctypes.c_size_t),
                ("ActiveProcessLimit", wintypes.DWORD),
                ("Affinity", ctypes.c_size_t),
                ("PriorityClass", wintypes.DWORD),
                ("SchedulingClass", wintypes.DWORD),
            ]

        class IoCounters(ctypes.Structure):
            _fields_ = [(name, ctypes.c_uint64) for name in (
                "ReadOperationCount", "WriteOperationCount", "OtherOperationCount",
                "ReadTransferCount", "WriteTransferCount", "OtherTransferCount",
            )]

        class ExtendedLimit(ctypes.Structure):
            _fields_ = [
                ("BasicLimitInformation", BasicLimit),
                ("IoInfo", IoCounters),
                ("ProcessMemoryLimit", ctypes.c_size_t),
                ("JobMemoryLimit", ctypes.c_size_t),
                ("PeakProcessMemoryUsed", ctypes.c_size_t),
                ("PeakJobMemoryUsed", ctypes.c_size_t),
            ]

        self._job_limit_type = ExtendedLimit
        signatures = {
            "QueryPerformanceFrequency": ([ctypes.POINTER(ctypes.c_int64)], wintypes.BOOL),
            "QueryPerformanceCounter": ([ctypes.POINTER(ctypes.c_int64)], wintypes.BOOL),
            "CreateToolhelp32Snapshot": ([wintypes.DWORD, wintypes.DWORD], wintypes.HANDLE),
            "Process32FirstW": ([wintypes.HANDLE, ctypes.POINTER(ProcessEntry)], wintypes.BOOL),
            "Process32NextW": ([wintypes.HANDLE, ctypes.POINTER(ProcessEntry)], wintypes.BOOL),
            "OpenProcess": ([wintypes.DWORD, wintypes.BOOL, wintypes.DWORD], wintypes.HANDLE),
            "GetProcessTimes": (
                [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4,
                wintypes.BOOL,
            ),
            "GetSystemTimes": ([ctypes.POINTER(wintypes.FILETIME)] * 3, wintypes.BOOL),
            "WaitForSingleObject": ([wintypes.HANDLE, wintypes.DWORD], wintypes.DWORD),
            "TerminateProcess": ([wintypes.HANDLE, wintypes.UINT], wintypes.BOOL),
            "CloseHandle": ([wintypes.HANDLE], wintypes.BOOL),
            "CreateJobObjectW": ([ctypes.c_void_p, wintypes.LPCWSTR], wintypes.HANDLE),
            "SetInformationJobObject": (
                [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD], wintypes.BOOL
            ),
            "AssignProcessToJobObject": ([wintypes.HANDLE, wintypes.HANDLE], wintypes.BOOL),
            "SetProcessAffinityMask": ([wintypes.HANDLE, ctypes.c_size_t], wintypes.BOOL),
            "GetActiveProcessorCount": ([wintypes.WORD], wintypes.DWORD),
        }
        for name, (arguments, result) in signatures.items():
            function = getattr(self._kernel, name)
            function.argtypes = arguments
            function.restype = result
        self._ntdll = ctypes.WinDLL("ntdll")
        self._ntdll.NtResumeProcess.argtypes = [wintypes.HANDLE]
        self._ntdll.NtResumeProcess.restype = wintypes.LONG
        frequency = ctypes.c_int64()
        if not self._kernel.QueryPerformanceFrequency(ctypes.byref(frequency)):
            raise ctypes.WinError(ctypes.get_last_error())
        if frequency.value <= 0:
            raise RuntimeError("QueryPerformanceFrequency returned a nonpositive frequency")
        self._frequency = frequency.value
        self.metadata = {
            "backend": "windows_toolhelp_process_times",
            "clock": "QueryPerformanceCounter",
            "clock_frequency_hz": self._frequency,
            "clock_resolution_ns": (1_000_000_000 + self._frequency - 1) // self._frequency,
            "cpu_counter_unit_ns": 100,
            "cpu_accounting_resolution": "OS accounting dependent",
            "logical_cpus": os.cpu_count(),
            "process_identity": "pid_and_creation_FILETIME",
            "process_name": "Toolhelp32_szExeFile",
            "host_cpu": "GetSystemTimes_kernel+user-idle",
            "termination": "creation_checked_handle_TerminateProcess",
            "snapshot_timestamp": "after_counter_reads",
            "snapshot_scope": "Toolhelp32 processes; sequential non-atomic scan",
            "inaccessible_processes": "omitted",
            "processor_group_caveat": "GetSystemTimes covers the caller's primary group on hosts with more than 64 processors",
            "launch_cleanup": "owned_job_KILL_ON_JOB_CLOSE",
            "launch_affinity": "suspended_process_before_resume; one processor group only",
        }

    @staticmethod
    def _filetime(value) -> int:
        return (value.dwHighDateTime << 32) | value.dwLowDateTime

    def clock_ns(self) -> int:
        counter = self._ctypes.c_int64()
        if not self._kernel.QueryPerformanceCounter(self._ctypes.byref(counter)):
            raise self._ctypes.WinError(self._ctypes.get_last_error())
        if counter.value < 0:
            raise RuntimeError("QueryPerformanceCounter returned a negative counter")
        return counter.value * 1_000_000_000 // self._frequency

    def _times(self, handle) -> tuple[int, int] | None:
        creation, exit_time, kernel, user = [self._filetime_type() for _ in range(4)]
        if not self._kernel.GetProcessTimes(
            handle,
            self._ctypes.byref(creation),
            self._ctypes.byref(exit_time),
            self._ctypes.byref(kernel),
            self._ctypes.byref(user),
        ):
            return None
        return self._filetime(creation), (self._filetime(kernel) + self._filetime(user)) * 100

    def snapshot(self) -> Snapshot:
        handle = self._kernel.CreateToolhelp32Snapshot(0x00000002, 0)
        if handle == self._ctypes.c_void_p(-1).value:
            raise self._ctypes.WinError(self._ctypes.get_last_error())
        processes: dict[int, Process] = {}
        try:
            entry = self._entry_type()
            entry.dwSize = self._ctypes.sizeof(entry)
            found = self._kernel.Process32FirstW(handle, self._ctypes.byref(entry))
            while found:
                pid = entry.th32ProcessID
                process_handle = self._kernel.OpenProcess(0x1000, False, pid)
                if process_handle:
                    try:
                        times = self._times(process_handle)
                        if times is not None:
                            processes[pid] = {
                                "ppid": entry.th32ParentProcessID,
                                "start": times[0],
                                "cpu_ns": times[1],
                                "name": entry.szExeFile,
                            }
                    finally:
                        self._kernel.CloseHandle(process_handle)
                found = self._kernel.Process32NextW(handle, self._ctypes.byref(entry))
            error = self._ctypes.get_last_error()
            if error != 18:
                raise self._ctypes.WinError(error)
        finally:
            self._kernel.CloseHandle(handle)
        idle, kernel, user = [self._filetime_type() for _ in range(3)]
        if not self._kernel.GetSystemTimes(
            self._ctypes.byref(idle), self._ctypes.byref(kernel), self._ctypes.byref(user)
        ):
            raise self._ctypes.WinError(self._ctypes.get_last_error())
        busy = self._filetime(kernel) + self._filetime(user) - self._filetime(idle)
        return {
            "ns": self.clock_ns(),
            "processes": processes,
            "host_cpu_ns": busy * 100,
        }

    def alive(self, pid: int, start: int) -> bool:
        handle = self._kernel.OpenProcess(0x1000 | 0x00100000, False, pid)
        if not handle:
            return False
        try:
            times = self._times(handle)
            return (
                times is not None
                and times[0] == start
                and self._kernel.WaitForSingleObject(handle, 0) == 258
            )
        finally:
            self._kernel.CloseHandle(handle)

    def terminate(self, pid: int, start: int) -> bool:
        handle = self._kernel.OpenProcess(0x0001 | 0x1000 | 0x00100000, False, pid)
        if not handle:
            return False
        try:
            times = self._times(handle)
            if (
                times is None
                or times[0] != start
                or self._kernel.WaitForSingleObject(handle, 0) != 258
            ):
                return False
            return bool(self._kernel.TerminateProcess(handle, 1))
        finally:
            self._kernel.CloseHandle(handle)

    def launch(self, command, env, log, cpus) -> subprocess.Popen:
        if self._kernel.GetActiveProcessorCount(0xFFFF) > 64:
            raise NotImplementedError("Windows launch supports one processor group only")
        mask = None
        if cpus is not None:
            maximum = self._ctypes.sizeof(self._ctypes.c_size_t) * 8
            if not cpus or any(
                not isinstance(cpu, int) or cpu < 0 or cpu >= maximum for cpu in cpus
            ):
                raise ValueError("CPU affinity indices exceed the native affinity mask")
            mask = sum(1 << cpu for cpu in set(cpus))
        job = self._kernel.CreateJobObjectW(None, None)
        if not job:
            raise self._ctypes.WinError(self._ctypes.get_last_error())
        process = None
        try:
            limits = self._job_limit_type()
            limits.BasicLimitInformation.LimitFlags = 0x00002000
            if not self._kernel.SetInformationJobObject(
                job, 9, self._ctypes.byref(limits), self._ctypes.sizeof(limits)
            ):
                raise self._ctypes.WinError(self._ctypes.get_last_error())
            process = subprocess.Popen(
                command,
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=log,
                stderr=subprocess.STDOUT,
                creationflags=subprocess.CREATE_NEW_PROCESS_GROUP | 0x00000004,
            )
            handle = int(process._handle)
            if mask is not None and not self._kernel.SetProcessAffinityMask(handle, mask):
                raise self._ctypes.WinError(self._ctypes.get_last_error())
            if not self._kernel.AssignProcessToJobObject(job, handle):
                raise self._ctypes.WinError(self._ctypes.get_last_error())
            status = self._ntdll.NtResumeProcess(handle)
            if status < 0:
                raise OSError(f"NtResumeProcess failed with NTSTATUS 0x{status & 0xFFFFFFFF:08x}")
            self._jobs[process.pid] = (process, job)
            return process
        except BaseException:
            try:
                if process is not None:
                    self._kernel.TerminateProcess(int(process._handle), 1)
            finally:
                self._kernel.CloseHandle(job)
            if process is not None:
                process.wait(timeout=5)
            raise

    def close_launch(self, process: subprocess.Popen) -> None:
        launch = self._jobs.get(process.pid)
        if launch is None or launch[0] is not process:
            return
        _, job = self._jobs.pop(process.pid)
        if not self._kernel.CloseHandle(job):
            raise self._ctypes.WinError(self._ctypes.get_last_error())
