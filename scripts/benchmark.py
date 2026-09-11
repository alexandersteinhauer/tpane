#!/usr/bin/env python3
"""Serial, macOS/std-library tpane performance harness (no host VT emulation).

Example, run separately on an otherwise idle machine with identical options:
  python3 scripts/benchmark.py --binary /abs/baseline/tpane --output baseline.json
  python3 scripts/benchmark.py --binary target/release/tpane --output candidate.json
Quick protocol check (not a meaningful performance result):
  python3 scripts/benchmark.py --binary target/release/tpane --panes 1,8 \
      --trials 1 --keys 3 --idle-seconds .1 --settle-seconds .05 \
      --output-bytes 4096 --burst-keys 5 --output smoke.json

Latency is input write start -> receipt of ASCII response and following host
ESC[?2026l, NOT physical display/Ghostty GUI latency. CPU is libproc task total
user+system time for the tpane PID, including its main AND worker threads but
excluding the Python backends and this driver. Output MiB/s is finite child
payload -> completed final host frame, not host transport bandwidth; intermediate
frames may be coalesced. Latency-phase CPU includes deterministic think time.

Keep binary build profile, options, OS, hardware, power/thermal state and external
load equal when comparing variants. All trials are raw observations; no synthetic
results or subtraction of driver overhead. A per-user advisory lock prevents
concurrent copies of this harness, NOT unrelated CPU tests. Do not run other
benchmarks concurrently. No user configuration is inherited. Only the requested
JSON, temporary fixture/HOME, and advisory lock file are written.
"""

import argparse
import ctypes
import datetime
import errno
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import pty
import random
import re
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import tty


SYNC_END = b"\x1b[?2026l"
READY = re.compile(rb"TPB_READY_(\d{10})_(\d{3})x(\d{3})_END")


class TaskInfo(ctypes.Structure):
    # Darwin proc_taskinfo, matching libc crate src/unix/bsd/apple/mod.rs.
    # The FIRST TWO fields are memory sizes, NOT CPU times. total_user/system
    # are Mach absolute-time ticks, already accumulated over all threads (do not
    # also add pti_threads_user/system, which describe live threads). Convert
    # using mach_timebase_info: assuming raw nanoseconds undercounts Apple Silicon
    # CPU (e.g. 125/3 ns per tick). Verify against Python process_time_ns below.
    _fields_ = [(name, ctypes.c_uint64) for name in (
        "pti_virtual_size", "pti_resident_size", "pti_total_user",
        "pti_total_system", "pti_threads_user", "pti_threads_system",
    )] + [(name, ctypes.c_int32) for name in (
        "pti_policy", "pti_faults", "pti_pageins", "pti_cow_faults",
        "pti_messages_sent", "pti_messages_received", "pti_syscalls_mach",
        "pti_syscalls_unix", "pti_csw", "pti_threadnum", "pti_numrunning",
        "pti_priority",
    )]


class TaskCPU:
    def __init__(self):
        self.lib = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
        self.lib.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int,
                                         ctypes.c_uint64, ctypes.c_void_p,
                                         ctypes.c_int]
        self.lib.proc_pidinfo.restype = ctypes.c_int
        if ctypes.sizeof(TaskInfo) != 96:
            raise RuntimeError("Unexpected Darwin proc_taskinfo ABI size")

        class Timebase(ctypes.Structure):
            _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]

        system = ctypes.CDLL("/usr/lib/libSystem.B.dylib")
        system.mach_timebase_info.argtypes = [ctypes.POINTER(Timebase)]
        system.mach_timebase_info.restype = ctypes.c_int
        timebase = Timebase()
        if system.mach_timebase_info(ctypes.byref(timebase)) or not timebase.denom:
            raise RuntimeError("mach_timebase_info failed")
        self.numer, self.denom = timebase.numer, timebase.denom
        # No busy-loop calibration: compare accumulated driver CPU at startup.
        # process_time_ns uses an independent OS process CPU clock. Allow sampling
        # skew but fail rather than publish results with an unexpected time ABI.
        own = self.snapshot(os.getpid())
        reference = time.process_time_ns()
        measured = own["user_ns"] + own["system_ns"]
        if abs(measured - reference) > max(2_000_000, reference * .05):
            raise RuntimeError(f"libproc CPU units failed process_time_ns check: "
                               f"converted={measured}, reference={reference}")
        self.validation = {"mach_timebase_numer": self.numer,
                           "mach_timebase_denom": self.denom,
                           "driver_libproc_cpu_ns": measured,
                           "driver_process_time_ns": reference,
                           "tolerance": "max(2ms, 5%); no busy-loop calibration"}

    def snapshot(self, pid):
        info = TaskInfo()
        n = self.lib.proc_pidinfo(pid, 4, 0, ctypes.byref(info),
                                 ctypes.sizeof(info))  # PROC_PIDTASKINFO = 4
        if n != ctypes.sizeof(info):
            raise OSError(ctypes.get_errno(), f"proc_pidinfo({pid}) returned {n}")
        if info.pti_threadnum < 1:
            raise RuntimeError("Invalid task thread count")
        return {"sample_ns": time.perf_counter_ns(),
                "user_ticks": info.pti_total_user,
                "system_ticks": info.pti_total_system,
                "user_ns": info.pti_total_user * self.numer // self.denom,
                "system_ns": info.pti_total_system * self.numer // self.denom,
                "threads": info.pti_threadnum,
                "resident_bytes": info.pti_resident_size,
                "virtual_bytes": info.pti_virtual_size}


def command_output(args):
    return subprocess.check_output(args, text=True, timeout=5).strip()


def digest(path):
    h = hashlib.sha256()
    with open(path, "rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def percentile(values, fraction):
    """Linear interpolation at (n-1)*fraction, also defined for n=1."""
    ordered = sorted(values)
    position = (len(ordered) - 1) * fraction
    lo = math.floor(position)
    hi = math.ceil(position)
    return ordered[lo] + (ordered[hi] - ordered[lo]) * (position - lo)


class HostStream:
    def __init__(self, fd, process):
        self.fd = fd
        self.process = process
        self.buffer = b""
        self.bytes_read = 0
        self.last_read_ns = 0

    def read(self, timeout):
        readable, _, _ = select.select([self.fd], [], [], max(0, timeout))
        if not readable:
            if self.process.poll() is not None:
                raise RuntimeError(f"tpane exited: {self.process.returncode}")
            return
        try:
            data = os.read(self.fd, 65536)
        except OSError as exc:
            if exc.errno == errno.EIO:
                raise RuntimeError("Outer PTY closed") from exc
            raise
        self.last_read_ns = time.perf_counter_ns()
        if not data:
            raise RuntimeError("Outer PTY EOF")
        self.bytes_read += len(data)
        # No terminal emulation or unbounded output capture. At 100x30 a frame
        # is far smaller than this cap, including ANSI styles and split markers.
        self.buffer = (self.buffer + data)[-1024 * 1024:]

    def wait(self, pattern, timeout):
        deadline = time.monotonic() + timeout
        while True:
            match = pattern.search(self.buffer)
            if match:
                end = self.buffer.find(SYNC_END, match.end())
                if end >= 0:
                    self.buffer = self.buffer[end + len(SYNC_END):]
                    return match, self.last_read_ns
            else:
                # Already-searched complete frames cannot contain a future marker.
                end = self.buffer.rfind(SYNC_END)
                if end >= 0:
                    self.buffer = self.buffer[end + len(SYNC_END):]
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                tail = repr(self.buffer[-400:])
                raise TimeoutError(f"No completed frame for {pattern.pattern!r}; tail={tail}")
            self.read(remaining)

    def drain(self, seconds):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            self.read(deadline - time.monotonic())
            self.buffer = b""
        self.buffer = b""

    def write(self, data):
        # All benchmark input is <=1024 bytes: a single write is intentional.
        start = time.perf_counter_ns()
        if os.write(self.fd, data) != len(data):
            raise RuntimeError("Short PTY input write; trial invalid")
        return start


def process_table():
    rows = command_output(["/bin/ps", "-axo", "pid=,ppid="])
    return [tuple(map(int, line.split())) for line in rows.splitlines()]


def cleanup(process, known_children, stream):
    # Backend shells own separate sessions; killing just tpane's process group
    # is insufficient. Discover descendants before terminating the app, including
    # a child that timed out before its READY was rendered.
    descendants = set(known_children)
    if process.poll() is None:
        try:
            table = process_table()
            ancestors = {process.pid} | descendants
            while True:
                found = {pid for pid, ppid in table if ppid in ancestors}
                if found <= ancestors:
                    break
                ancestors |= found
            descendants |= ancestors - {process.pid}
        except (OSError, subprocess.SubprocessError):
            pass
    targets = descendants | ({process.pid} if process.poll() is None else set())
    for sig, duration in ((signal.SIGTERM, 2.0), (signal.SIGKILL, 1.0)):
        for pid in targets:
            try:
                os.kill(pid, sig)
            except ProcessLookupError:
                pass
        deadline = time.monotonic() + duration
        # Drain while waiting so shutdown cannot deadlock on a full outer PTY.
        while time.monotonic() < deadline:
            process.poll()  # reap tpane promptly
            live = set()
            for pid in targets:
                try:
                    os.kill(pid, 0)
                    live.add(pid)
                except ProcessLookupError:
                    pass
            targets = live
            if not targets:
                break
            try:
                stream.read(min(.05, max(0, deadline - time.monotonic())))
            except (RuntimeError, OSError):
                time.sleep(.01)
        if not targets:
            break
    try:
        process.wait(timeout=.1)
    except subprocess.TimeoutExpired:
        raise RuntimeError("tpane did not exit after SIGKILL")


def run_trial(args, panes, index, cpu, fixture_source, think_ms):
    result = {"panes": panes, "trial": index, "phases": {}}
    with tempfile.TemporaryDirectory(prefix="tpane-benchmark-") as directory:
        root = Path(directory)
        home = root / "home"
        home.mkdir()
        stub = root / "benchmark-child.py"
        # Absolute interpreter + absolute --shell, independent of PATH/login
        # setup. Preserve the fixture body, ignoring tpane's '-l' shell argument.
        stub.write_text(f"#!{sys.executable}\n" + fixture_source.split("\n", 1)[1])
        stub.chmod(0o700)
        env = {"HOME": str(home), "PWD": str(root), "SHELL": str(stub),
               "PATH": f"{Path(sys.executable).parent}:/usr/bin:/bin",
               "TERM": "xterm-256color", "COLORTERM": "truecolor",
               "LANG": "C", "LC_ALL": "C", "TMPDIR": str(root),
               "XDG_CONFIG_HOME": str(home / ".config"),
               "XDG_CACHE_HOME": str(home / ".cache"),
               "XDG_DATA_HOME": str(home / ".local/share"),
               "PYTHONNOUSERSITE": "1", "PYTHONDONTWRITEBYTECODE": "1",
               "TPANE_BENCH_BYTES": str(args.output_bytes)}
        master, slave = pty.openpty()
        process = None
        children = set()
        try:
            tty.setraw(slave, termios.TCSANOW)
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))

            def controlling_terminal():
                os.setsid()
                fcntl.ioctl(0, termios.TIOCSCTTY, 0)

            startup = time.perf_counter_ns()
            process = subprocess.Popen([str(args.binary), "--shell", str(stub)],
                                       stdin=slave, stdout=slave, stderr=slave,
                                       cwd=root, env=env, preexec_fn=controlling_terminal)
            stream = HostStream(master, process)
            os.close(slave)
            slave = None
            ready_records = []
            for pane in range(panes):
                if pane:
                    # Add siblings under the root, never nest under selected tab.
                    stream.write(b"\x1b[<0;100;1M\x1b[<0;100;1m")
                deadline = time.monotonic() + args.timeout
                while True:
                    match, stamp = stream.wait(READY, max(0, deadline - time.monotonic()))
                    pid, cols, rows = map(int, match.groups())
                    if pid not in children:
                        children.add(pid)
                        break
                expected_rows = 30 - args.root_bar_rows if pane == 0 else 28
                if (cols, rows) != (100, expected_rows):
                    raise RuntimeError(f"Unexpected child viewport {cols}x{rows}; "
                                       f"expected 100x{expected_rows}")
                ready_records.append({"pid": pid, "cols": cols, "rows": rows})
            result.update({"tpane_pid": process.pid, "children": ready_records,
                           "startup_and_pane_creation_ms": (stamp - startup) / 1e6,
                           "selected_viewport": [100, 30 - args.root_bar_rows if panes == 1 else 28],
                           "ps_before": command_output([
                               "/bin/ps", "-p", str(process.pid), "-o",
                               "pid=,ppid=,time=,comm="])})
            actual = {child for child, parent in process_table() if parent == process.pid}
            if actual != children:
                raise RuntimeError(f"Pane PID verification failed: ready={children}, ps={actual}")
            stream.drain(args.settle_seconds)

            def measure(name, workload):
                before = cpu.snapshot(process.pid)
                host_before = stream.bytes_read
                wall_start = time.perf_counter_ns()
                driver_start = time.process_time_ns()
                details = workload()
                driver_cpu = time.process_time_ns() - driver_start
                wall_end = time.perf_counter_ns()
                after = cpu.snapshot(process.pid)
                user = after["user_ns"] - before["user_ns"]
                system = after["system_ns"] - before["system_ns"]
                if min(user, system) < 0:
                    raise RuntimeError("Non-monotonic libproc task CPU counters")
                cpu_wall = (after["sample_ns"] - before["sample_ns"]) / 1e9
                result["phases"][name] = {
                    "wall_seconds": (wall_end - wall_start) / 1e9,
                    "driver_cpu_seconds": driver_cpu / 1e9,
                    "cpu_user_seconds": user / 1e9,
                    "cpu_system_seconds": system / 1e9,
                    "cpu_total_seconds": (user + system) / 1e9,
                    "cpu_sample_wall_seconds": cpu_wall,
                    "cpu_percent_one_core": (user + system) / 1e9 / cpu_wall * 100,
                    "cpu_before": before, "cpu_after": after,
                    "host_bytes_read": stream.bytes_read - host_before,
                    **(details or {}),
                }

            measure("idle", lambda: stream.drain(args.idle_seconds))
            counter = 0

            def ack(n):
                return re.compile(f"TPB_ACK_{pid:010d}_{n:08d}_END".encode("ascii"))

            def latency():
                nonlocal counter
                samples = []
                for delay in think_ms:
                    # Fixed seeded schedule is identical for every trial/variant.
                    stream.drain(delay / 1000)
                    counter += 1
                    pattern = ack(counter)  # compile outside latency interval
                    start = stream.write(b"x")
                    _, end = stream.wait(pattern, args.timeout)
                    samples.append((end - start) / 1e6)
                return {"samples_ms": samples, "think_times_ms": think_ms,
                        "p50_ms": percentile(samples, .50),
                        "p95_ms": percentile(samples, .95)}

            measure("input_to_render", latency)

            def burst():
                nonlocal counter
                counter += args.burst_keys
                pattern = ack(counter)
                payload = b"x" * args.burst_keys
                start = stream.write(payload)
                _, end = stream.wait(pattern, args.timeout)
                return {"keys": args.burst_keys, "last_ack": counter,
                        "catchup_ms": (end - start) / 1e6,
                        "protocol": "unbracketed ASCII paste/burst, one write"}

            measure("paste_burst", burst)

            def output():
                pattern = re.compile(f"TPB_DONE_{pid:010d}_00000001_END".encode("ascii"))
                start = stream.write(b"b")
                _, end = stream.wait(pattern, args.timeout)
                elapsed = (end - start) / 1e9
                return {"payload_bytes": args.output_bytes,
                        "completion_seconds": elapsed,
                        "payload_mib_per_second": args.output_bytes / 1048576 / elapsed}

            measure("finite_output", output)
            result["ps_after"] = command_output([
                "/bin/ps", "-p", str(process.pid), "-o", "pid=,ppid=,time=,comm="])
            return result
        finally:
            try:
                if process is not None:
                    cleanup(process, children, stream)
            finally:
                if slave is not None:
                    os.close(slave)
                os.close(master)


def arguments():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--panes", default="1,8,24")
    parser.add_argument("--root-bar-rows", type=int, choices=(0, 1), default=1,
                        help="use 0 only for the pre-tabs single-terminal baseline")
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--output", type=Path, help="JSON file; default stdout")
    parser.add_argument("--keys", type=int, default=60)
    parser.add_argument("--burst-keys", type=int, default=100)
    parser.add_argument("--output-bytes", type=int, default=2 * 1024 * 1024)
    parser.add_argument("--idle-seconds", type=float, default=3.0)
    parser.add_argument("--settle-seconds", type=float, default=.3)
    parser.add_argument("--timeout", type=float, default=30.0,
                        help="per-child startup / per-response timeout")
    parser.add_argument("--seed", type=int, default=2026)
    args = parser.parse_args()
    try:
        args.panes = [int(n) for n in args.panes.split(",")]
    except ValueError:
        parser.error("--panes must be comma-separated integers")
    if not args.panes or min(args.panes) < 1 or len(set(args.panes)) != len(args.panes):
        parser.error("--panes must contain unique positive counts")
    if args.root_bar_rows == 0 and args.panes != [1]:
        parser.error("the pre-tabs baseline supports only --panes 1")
    if min(args.trials, args.keys, args.burst_keys, args.output_bytes) < 1:
        parser.error("trials, keys, burst-keys and output-bytes must be positive")
    if args.burst_keys > 1024 or args.keys + args.burst_keys >= 100000000:
        parser.error("burst-keys must be <=1024 and total ACKs <100000000")
    if (not all(math.isfinite(v) for v in (args.idle_seconds, args.settle_seconds, args.timeout))
            or args.idle_seconds <= 0 or args.settle_seconds < 0 or args.timeout <= 0):
        parser.error("idle/timeout must be finite positive; settle finite nonnegative")
    args.binary = args.binary.resolve(strict=True)
    if not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        parser.error("--binary must be an executable file")
    if any(c.isspace() for c in sys.executable):
        parser.error("Python executable path must not contain whitespace (fixture shebang)")
    if args.output and args.output.resolve() in (
            args.binary, Path(__file__).resolve(), Path(__file__).with_name("benchmark-child.py").resolve()):
        parser.error("--output must not overwrite the binary or harness")
    return args


def main():
    args = arguments()
    if sys.platform != "darwin":
        raise SystemExit("This harness requires macOS libproc")
    # Lock outside per-trial directories, do not unlink (avoids inode races).
    lock_path = Path("/tmp") / f"tpane-benchmark-{os.getuid()}.lock"
    with open(lock_path, "a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise SystemExit("Another tpane benchmark is running; CPU tests must be serial")
        print("Run on an otherwise idle machine; lock excludes only other harness copies.",
              file=sys.stderr)
        cpu = TaskCPU()
        fixture = Path(__file__).with_name("benchmark-child.py")
        source = fixture.read_text()
        rng = random.Random(args.seed)
        think_ms = [rng.randrange(21) for _ in range(args.keys)]
        report = {
            "schema_version": 1,
            "status": "running",
            "environment": {
                "utc_started": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                "platform": platform.platform(), "machine": platform.machine(),
                "macos": platform.mac_ver()[0], "python": sys.version,
                "python_executable": sys.executable, "logical_cpus": os.cpu_count(),
                "hardware_model": command_output(["/usr/sbin/sysctl", "-n", "hw.model"]),
                "cpu_brand": command_output(["/usr/sbin/sysctl", "-n", "machdep.cpu.brand_string"]),
                "binary": str(args.binary), "binary_sha256": digest(args.binary),
                "binary_size": args.binary.stat().st_size,
                "cpu_counter_validation": cpu.validation,
                "harness_sha256": digest(Path(__file__)), "fixture_sha256": digest(fixture),
                "outer_pty": {"cols": 100, "rows": 30, "pixel_width": 0, "pixel_height": 0},
                "host_term": "xterm-256color", "host_colorterm": "truecolor",
                "environment_policy": "allowlist; temporary HOME/PWD/SHELL/XDG/TMPDIR; no user configs",
            },
            "options": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
            "method": {
                "latency": "write start to ASCII ACK plus following ESC[?2026l received on outer PTY",
                "percentiles": "linear interpolation at (n-1)*p, per trial",
                "cpu": "PROC_PIDTASKINFO=4, 96-byte Darwin proc_taskinfo; pti_total_user/system Mach ticks converted to ns with mach_timebase_info",
                "cpu_scope": "tpane PID, all threads; excludes Python children and benchmark driver",
                "memory": "tpane PID resident and virtual bytes at phase boundaries, not peak or system-wide memory",
                "cpu_percent": "100% = one fully occupied core; may exceed 100%",
                "ps": "before/after PID, PPID, aggregate CPU time, command; coarse corroboration only",
                "order": "trial-major, pane counts in CLI order; fresh process per case",
                "limitations": ["not physical display or Ghostty GUI latency",
                                "driver scheduling, PTY transport and Python backend affect wall time",
                                "finite-output throughput includes backend production and final render; frames may coalesce",
                                "latency CPU includes 0-20ms seeded think times",
                                "1 pane has 29 terminal rows; multiple panes have 28 (two bars)",
                                "CPU sample boundaries include small libproc-call overhead",
                                "no control over unrelated load, thermals, power state or binary build flags",
                                "ps CPU time is coarse; ABI/thread count/monotonicity checked; timebase conversion checked against driver process_time_ns"],
            },
            "trials": [],
        }

        def save():
            text = json.dumps(report, indent=2, allow_nan=False) + "\n"
            if args.output:
                args.output.write_text(text)
            else:
                sys.stdout.write(text)

        # Make SIGTERM follow the same bounded cleanup path as Ctrl-C.
        def interrupted(signum, frame):
            raise KeyboardInterrupt(f"signal {signum}")

        previous = signal.signal(signal.SIGTERM, interrupted)
        try:
            for trial in range(1, args.trials + 1):
                for panes in args.panes:
                    print(f"trial {trial}/{args.trials}, panes={panes}", file=sys.stderr)
                    report["active_case"] = {"trial": trial, "panes": panes}
                    report["trials"].append(run_trial(args, panes, trial, cpu, source, think_ms))
                    del report["active_case"]
                    if args.output:
                        save()
            report["environment"]["binary_sha256_after"] = digest(args.binary)
            if report["environment"]["binary_sha256_after"] != report["environment"]["binary_sha256"]:
                raise RuntimeError("Binary changed during benchmark; comparison is invalid")
            report["status"] = "complete"
        except (Exception, KeyboardInterrupt) as exc:
            report["status"] = "failed"
            report["error"] = f"{type(exc).__name__}: {exc}"
            save()
            raise
        finally:
            signal.signal(signal.SIGTERM, previous)
        save()


if __name__ == "__main__":
    main()
