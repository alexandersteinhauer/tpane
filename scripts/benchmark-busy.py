#!/usr/bin/env python3
"""macOS multi-busy-pane benchmark; run serially on an otherwise idle machine.

Example: --binary target/release/tpane --panes 8 --busy 4 --output busy.json
Repeat with --panes 24 --busy 8 and identical options/build/power/load conditions.
ACK latency ends at the completed host frame, not physical display. Throughput
counts producer bytes accepted by write(), INCLUDING PTY/application buffers;
not necessarily parsed/rendered during the window. Post-stop DONE verification
is separate and proves ordered processing, not presentation of every log line.
"""

import argparse
import datetime
import fcntl
import json
import math
import os
from pathlib import Path
import platform
import pty
import re
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import tty

sys.dont_write_bytecode = True
from benchmark import (TaskCPU, HostStream, READY, cleanup, command_output,
                       digest, percentile, process_table, select_pane, TPANE_NEW_SIBLING)


class CheckedStream(HostStream):
    """Check wire output even when HostStream.wait/drain discards old frames."""
    watch_hidden = False
    tail = b""

    def read(self, timeout):
        previous = self.bytes_read
        super().read(timeout)
        count = self.bytes_read - previous
        if count:
            data = self.tail + self.buffer[-count:]
            if self.watch_hidden and (b"TPB_BUSY_" in data or b"TPB_DONE_" in data):
                raise RuntimeError("Hidden producer output leaked into active host stream")
            self.tail = data[-80:]


def marker(kind, pid, count):
    return re.compile(f"TPB_{kind}_{pid:010d}_{count:08d}_END".encode())


def run_trial(args, result, cpu, source):
    with tempfile.TemporaryDirectory(prefix="tpane-busy-") as directory:
        root = Path(directory)
        home = root / "home"
        home.mkdir()
        stub = root / "benchmark-busy-child.py"
        stub.write_text(f"#!{sys.executable}\n" + source.split("\n", 1)[1])
        stub.chmod(0o700)
        env = {"HOME": str(home), "PWD": str(root), "SHELL": str(stub),
               "PATH": f"{Path(sys.executable).parent}:/usr/bin:/bin",
               "TERM": "xterm-256color", "COLORTERM": "truecolor",
               "LANG": "C", "LC_ALL": "C", "TMPDIR": str(root),
               "XDG_CONFIG_HOME": str(home / ".config"),
               "XDG_CACHE_HOME": str(home / ".cache"),
               "XDG_DATA_HOME": str(home / ".local/share"),
               "PYTHONNOUSERSITE": "1", "PYTHONDONTWRITEBYTECODE": "1"}
        master, slave = pty.openpty()
        process = None
        children = set()
        try:
            tty.setraw(slave, termios.TCSANOW)
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 28, 100, 0, 0))

            def controlling_terminal():
                os.setsid()
                fcntl.ioctl(0, termios.TIOCSCTTY, 0)

            process = subprocess.Popen([str(args.binary), "--shell", str(stub)],
                                       stdin=slave, stdout=slave, stderr=slave,
                                       cwd=root, env=env, preexec_fn=controlling_terminal)
            stream = CheckedStream(master, process)
            os.close(slave)
            slave = None
            records = result["children"] = []
            result["tpane_pid"] = process.pid
            result["executed_fixture_sha256"] = digest(stub)
            for pane in range(args.panes):
                if pane:
                    stream.write(TPANE_NEW_SIBLING)
                deadline = time.monotonic() + args.timeout
                while True:
                    match, _ = stream.wait(READY, max(0, deadline - time.monotonic()))
                    pid, cols, rows = map(int, match.groups())
                    if pid not in children:
                        children.add(pid)
                        break
                if (cols, rows) != (100, 28):
                    raise RuntimeError(f"Unexpected viewport: {cols}x{rows}")
                records.append({"pid": pid, "ready_viewport": [cols, rows]})
            actual = {pid for pid, parent in process_table() if parent == process.pid}
            if actual != children:
                raise RuntimeError(f"Child ownership mismatch: {actual} != {children}")
            producers = [r["pid"] for r in records[:args.busy]]
            responder = records[-1]["pid"]
            result.update(producers=producers, responder=responder)
            stream.drain(.2)
            stream.watch_hidden = True
            before = cpu.snapshot(process.pid)
            host_before = stream.bytes_read
            start = time.perf_counter_ns()
            for pid in producers:
                os.kill(pid, signal.SIGUSR1)
            result["start_signal_window_ns"] = [start, time.perf_counter_ns()]
            samples = result["latency_samples"] = []
            deadline = start + int(args.seconds * 1e9)
            for n in range(1, args.keys + 1):
                due = start + int((n - .5) * args.seconds / args.keys * 1e9)
                stream.drain(max(0, (due - time.perf_counter_ns()) / 1e9))
                pattern = marker("ACK", responder, n)
                sent = stream.write(b"x")
                remaining = (deadline - sent) / 1e9
                if remaining <= 0:
                    raise TimeoutError("ACK workload exceeded requested production window")
                _, end = stream.wait(pattern, min(args.timeout, remaining))
                samples.append({"key": n, "scheduled_ns": due, "write_ns": sent,
                                "frame_ns": end, "latency_ms": (end - sent) / 1e6})
            stream.drain(max(0, (deadline - time.perf_counter_ns()) / 1e9))
            stop = time.perf_counter_ns()
            for pid in producers:
                os.kill(pid, signal.SIGUSR2)
            stop_end = time.perf_counter_ns()
            after = cpu.snapshot(process.pid)
            user = after["user_ns"] - before["user_ns"]
            system = after["system_ns"] - before["system_ns"]
            if min(user, system) < 0:
                raise RuntimeError("Non-monotonic task CPU counters")
            wall = (after["sample_ns"] - before["sample_ns"]) / 1e9
            latencies = [s["latency_ms"] for s in samples]
            result["steady"] = {
                "start_ns": start, "stop_signal_window_ns": [stop, stop_end],
                "wall_seconds": (stop - start) / 1e9,
                "cpu_before": before, "cpu_after": after,
                "cpu_user_seconds": user / 1e9, "cpu_system_seconds": system / 1e9,
                "cpu_total_seconds": (user + system) / 1e9,
                "cpu_sample_wall_seconds": wall,
                "cpu_percent_one_core": (user + system) / 1e9 / wall * 100,
                "host_bytes_read": stream.bytes_read - host_before,
                "p50_ms": percentile(latencies, .5), "p95_ms": percentile(latencies, .95)}
            # Check final ACK while all stopped producers remain hidden, before
            # intentionally making their DONE markers visible for drain checks.
            sent = stream.write(b"x")
            _, end = stream.wait(marker("ACK", responder, args.keys + 1), args.timeout)
            result["post_stop_ack_ms"] = (end - sent) / 1e6
            result["hidden_output_check"] = "passed: no BUSY/DONE wire markers while hidden"
            stream.watch_hidden = False
            drains = result["drain_samples"] = []
            stats = result["producer_stats"] = []
            for index, pid in enumerate(producers):
                stream.buffer = b""
                selected = select_pane(stream, index)
                _, end = stream.wait(marker("DONE", pid, 1), args.timeout)
                drains.append({"pid": pid, "selected_ns": selected, "done_frame_ns": end,
                               "selection_to_done_ms": (end - selected) / 1e6,
                               "stop_to_done_ms": (end - stop) / 1e6})
                stat = json.loads((root / f"busy-{pid}.json").read_text())
                stats.append(stat)
                # Python <3.10 on macOS may use different per-process clock
                # origins. Validate child ordering, never compare to driver ns.
                if (stat["pid"] != pid or stat["generation"] != 1 or
                        stat["accepted_bytes"] <= 0 or stat["window_seconds"] <= 0 or
                        not 0 < stat["start_ns"] < stat["stop_ns"]):
                    raise RuntimeError(f"Invalid producer stats: {stat}")
            accepted = sum(s["accepted_bytes"] for s in stats)
            result["steady"].update(
                producer_accepted_bytes=accepted,
                producer_accepted_mib_per_second=accepted / 1048576 / ((stop - start) / 1e9),
                backend_cpu_seconds=sum(s["backend_cpu_seconds"] for s in stats))
            result["drain_complete_ms"] = (end - stop) / 1e6
        finally:
            try:
                if process is not None:
                    cleanup(process, children, stream)
            finally:
                if slave is not None:
                    os.close(slave)
                os.close(master)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--panes", type=int, default=8)
    parser.add_argument("--busy", type=int, default=4)
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--seconds", type=float, default=5)
    parser.add_argument("--keys", type=int, default=80)
    parser.add_argument("--timeout", type=float, default=30,
                        help="startup, ACK and per-pane post-stop drain timeout")
    parser.add_argument("--output", type=Path, help="JSON path; default stdout")
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("requires macOS libproc")
    if not 1 <= args.busy < args.panes or min(args.trials, args.keys) < 1 or args.keys >= 99999999:
        parser.error("require 1 <= busy < panes, positive trials/keys, keys < 99999999")
    if not all(math.isfinite(v) and v > 0 for v in (args.seconds, args.timeout)):
        parser.error("seconds/timeout must be finite positive")
    args.binary = args.binary.resolve(strict=True)
    if not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        parser.error("binary must be executable")
    if any(c.isspace() for c in sys.executable):
        parser.error("Python executable cannot contain whitespace (fixture shebang)")
    fixture = Path(__file__).with_name("benchmark-busy-child.py")
    shared = Path(__file__).with_name("benchmark.py")
    protected = [args.binary, Path(__file__), fixture, shared,
                 Path(__file__).with_name("benchmark-child.py")]
    if args.output and args.output.resolve() in [p.resolve() for p in protected]:
        parser.error("output must not overwrite binary or harness/fixtures")
    lock_path = Path("/tmp") / f"tpane-benchmark-{os.getuid()}.lock"
    with open(lock_path, "a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            parser.error("another tpane benchmark is running; tests must be serial")
        cpu = TaskCPU()
        report = {
            "schema_version": 1, "benchmark": "multi-busy-pane", "status": "running",
            "options": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
            "environment": {
                "utc_started": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                "platform": platform.platform(), "macos": platform.mac_ver()[0],
                "machine": platform.machine(), "logical_cpus": os.cpu_count(),
                "python": sys.version, "python_executable": sys.executable,
                "hardware_model": command_output(["/usr/sbin/sysctl", "-n", "hw.model"]),
                "cpu_brand": command_output(["/usr/sbin/sysctl", "-n", "machdep.cpu.brand_string"]),
                "binary_sha256": digest(args.binary), "binary_size": args.binary.stat().st_size,
                "fixture_sha256": digest(fixture), "harness_sha256": digest(Path(__file__)),
                "shared_harness_sha256": digest(shared), "cpu_counter_validation": cpu.validation,
                "outer_pty": [100, 28], "selected_viewport": [100, 28],
                "environment_policy": "allowlist; isolated temporary HOME/cwd/XDG/TMPDIR; no real configs or clipboard",
                "term": "xterm-256color", "colorterm": "truecolor", "locale": "C"},
            "method": {
                "latency": "x write start -> ACK plus following ESC[?2026l; not physical display",
                "schedule": "keys evenly spaced at interval midpoints; serial ACK waits; fail on window overrun",
                "cpu": "libproc tpane PID total user+system Mach ticks, all threads; 100%=one core; excludes driver/backends",
                "memory_threads": "raw CPU snapshots include RSS/virtual bytes/thread count at boundaries, not peaks",
                "throughput": "sum successful producer payload write bytes / parent start-to-stop-signal seconds; INCLUDES PTY/application buffers, not processed/rendered throughput",
                "producer_windows": "raw per-child signal handling timestamps/windows; signal delivery skew included; backend process CPU optional corroboration",
                "clock": "perf_counter_ns origins are process-local on older macOS Python; compare durations, NEVER parent/child absolute timestamps",
                "drain": "sequential explorer selection after stop (NUL, Home to root, Down index+1, Enter); DONE completed frame proves prior accepted bytes processed; stop-to-DONE includes navigation and earlier panes' checks",
                "navigation": "NUL then n creates/selects each root sibling and dismisses explorer; final leaf is responder; no persistent bars",
                "hidden_check": "scan host wire for BUSY/DONE markers while responder selected; not a full VT screen emulator",
                "payload": "64-byte ASCII lines including CRLF; <=16KiB pending per producer; unsent bytes discarded at stop",
                "limitations": "Python production/driver scheduling/PTY transport affect results; no control over external load, thermals, power, or build flags",
                "lock": str(lock_path)}, "trials": []}

        def save():
            text = json.dumps(report, indent=2, allow_nan=False) + "\n"
            if args.output:
                args.output.write_text(text)
            else:
                sys.stdout.write(text)

        def interrupted(signum, frame):
            raise KeyboardInterrupt(f"signal {signum}")

        previous = signal.signal(signal.SIGTERM, interrupted)
        try:
            source = fixture.read_text()
            for index in range(1, args.trials + 1):
                print(f"busy trial {index}/{args.trials}: {args.panes}/{args.busy}; keep machine idle", file=sys.stderr)
                trial = {"trial": index, "status": "running"}
                report["trials"].append(trial)
                try:
                    run_trial(args, trial, cpu, source)
                    trial["status"] = "complete"
                except (Exception, KeyboardInterrupt) as exc:
                    trial.update(status="failed", error=f"{type(exc).__name__}: {exc}")
                    raise
                if args.output:
                    save()
            report["environment"]["binary_sha256_after"] = digest(args.binary)
            if report["environment"]["binary_sha256_after"] != report["environment"]["binary_sha256"]:
                raise RuntimeError("Binary changed during benchmark")
            report["status"] = "complete"
        except (Exception, KeyboardInterrupt) as exc:
            report.update(status="failed", error=f"{type(exc).__name__}: {exc}")
            save()
            raise
        finally:
            signal.signal(signal.SIGTERM, previous)
        save()


if __name__ == "__main__":
    main()
