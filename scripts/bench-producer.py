#!/usr/bin/env python3
"""Finite, driver-synchronized PTY producer (a shell's -l argument is ignored).

Required: TPANE_BENCH_READY_DIR and TPANE_BENCH_CONTROL (driver AF_UNIX/DGRAM
address). Commands are JSON {generation, payload_bytes, start_ns}; start_ns uses
POSIX CLOCK_MONOTONIC nanoseconds shared across processes on this host. Invalid, repeated, and in-flight commands are
silently discarded. Only a matching post-output cursor report produces a done
datagram. TPANE_BENCH_TIMEOUT bounds that report wait (seconds, default 30).
"""

import json
import math
import os
import re
import select
import signal
import socket
import tempfile
import termios
import time
import tty


CHUNK_BYTES = 16 * 1024


def clock_ns():
    # Unlike perf_counter on macOS Python 3.9, this epoch is process-independent.
    return time.clock_gettime_ns(time.CLOCK_MONOTONIC)


def output_block():
    """Same 64 KiB changing ASCII log block as benchmark-child.py."""
    line = b"2025-01-01T12:34:56 INFO request=012345 status=200 bytes=4096"
    line = line.ljust(62, b" ") + b"\r\n"
    return b"".join(line.replace(b"012345", f"{i:06d}".encode("ascii"))
                    for i in range(1024))


def payload_chunks(block, total):
    """Repeat the full block, including its exact prefix for a partial tail."""
    remaining = total
    while remaining:
        for offset in range(0, len(block), CHUNK_BYTES):
            chunk = block[offset:offset + min(CHUNK_BYTES, remaining)]
            yield chunk
            remaining -= len(chunk)
            if not remaining:
                return


def write_all(data):
    view = memoryview(data)
    while view:
        view = view[os.write(1, view):]


def parse_command(data, last_generation):
    try:
        command = json.loads(data)
    except (ValueError, UnicodeError, RecursionError):
        return None
    if not isinstance(command, dict):
        return None
    for field in ("generation", "payload_bytes", "start_ns"):
        if type(command.get(field)) is not int:
            return None
    if (command["generation"] <= last_generation or command["generation"] <= 0
            or command["payload_bytes"] <= 0 or command["start_ns"] < 0):
        return None
    return command


class CursorReports:
    """Incremental bounded parser; unrelated and mismatched reports are ignored."""

    pattern = re.compile(rb"\x1b\[([0-9]{1,9});([0-9]{1,9})R")

    def __init__(self):
        self.pending = b""

    def feed(self, data):
        self.pending += data
        reports = []
        end = 0
        for match in self.pattern.finditer(self.pending):
            reports.append((int(match[1]), int(match[2])))
            end = match.end()
        # No valid partial report can exceed 22 bytes.
        self.pending = self.pending[end:][-22:]
        return reports


def discard_input():
    if not os.read(0, 4096):
        raise EOFError


def discard_commands(control):
    while True:
        try:
            control.recv(65536)
        except BlockingIOError:
            return


def wait_until(start_ns, control):
    while True:
        remaining = start_ns - clock_ns()
        if remaining <= 0:
            return
        # Cap individual waits, including for malformedly distant deadlines.
        ready, _, _ = select.select([control, 0], [], [], min(remaining, 1_000_000_000) / 1e9)
        if control in ready:
            discard_commands(control)
        if 0 in ready:
            discard_input()


def wait_for_report(control, expected, timeout):
    parser = CursorReports()
    deadline = time.perf_counter() + timeout
    while True:
        remaining = deadline - time.perf_counter()
        if remaining <= 0:
            return None
        ready, _, _ = select.select([control, 0], [], [], remaining)
        if control in ready:
            discard_commands(control)
        if 0 in ready:
            data = os.read(0, 4096)
            if not data:
                raise EOFError
            if expected in parser.feed(data):
                return clock_ns()


def send_done(control, driver, record, timeout):
    data = json.dumps(record).encode("ascii")
    deadline = time.monotonic() + timeout
    while True:
        try:
            if control.sendto(data, driver) != len(data):
                raise RuntimeError("Short completion datagram")
            return
        except BlockingIOError:
            # Many producers can finish together; don't drop a parser proof
            # merely because the driver's datagram receive queue is full.
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([], [control], [], remaining)[1]:
                raise TimeoutError("Driver did not drain producer completions")


def produce(control, driver, command, block, timeout):
    wait_until(command["start_ns"], control)
    size = os.get_terminal_size(0)  # Tabs may have resized this PTY since READY.
    started_ns = clock_ns()
    write_all(b"\x1b[2J\x1b[1;1H")
    for chunk in payload_chunks(block, command["payload_bytes"]):
        write_all(chunk)
    # Discard queued commands and stale reports/keys before issuing our query.
    discard_commands(control)
    while select.select([0], [], [], 0)[0]:
        discard_input()
    pid = os.getpid()
    marker = f"TPB_DONE_{pid:010d}_{command['generation']:08d}_END"
    expected = (1, len(marker) + 1)
    write_all((f"\x1b[2J\x1b[1;1H{marker}\x1b[6n").encode("ascii"))
    completed_ns = wait_for_report(control, expected, timeout)
    discard_commands(control)  # Never defer an overlapping phase until idle.
    if completed_ns is not None:
        send_done(control, driver, {
            "kind": "done", "pid": pid, "generation": command["generation"],
            "payload_bytes": command["payload_bytes"], "started_ns": started_ns,
            "completed_ns": completed_ns, "cols": size.columns, "rows": size.lines,
            "dsr": list(expected),
        }, timeout)


def publish_geometry(directory, pid, size):
    fd, temporary = tempfile.mkstemp(prefix=f".child-{pid}-", dir=directory)
    try:
        with os.fdopen(fd, "w") as record:
            record.write(f"{size.columns} {size.lines}\n")
        os.replace(temporary, os.path.join(directory, f"child-{pid}"))
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def main():
    directory = os.environ["TPANE_BENCH_READY_DIR"]
    driver = os.environ["TPANE_BENCH_CONTROL"]
    timeout = float(os.environ.get("TPANE_BENCH_TIMEOUT", "30"))
    if not math.isfinite(timeout) or timeout <= 0:
        raise ValueError("TPANE_BENCH_TIMEOUT must be finite and positive")
    block = output_block()  # Prepare all log content before publishing READY.
    pid = os.getpid()
    address = os.path.join(directory, f"producer-{pid}.sock")
    control = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
    original = termios.tcgetattr(0)
    bound = False
    try:
        control.bind(address)
        bound = True
        os.chmod(address, 0o600)
        control.setblocking(False)
        tty.setraw(0, termios.TCSANOW)
        size = os.get_terminal_size(0)
        publish_geometry(directory, pid, size)
        write_all((f"\x1b[2J\x1b[1;1HTPB_READY_{pid:010d}_"
                   f"{size.columns:03d}x{size.lines:03d}_END").encode("ascii"))
        count = 0
        last_generation = 0
        while True:
            ready, _, _ = select.select([control, 0], [], [])
            if 0 in ready:
                data = os.read(0, 4096)
                if not data:
                    return
                for key in data:
                    if key == ord("q"):
                        return
                    if key == ord("x"):
                        count += 1
                        write_all((f"\x1b[1;1HTPB_ACK_{pid:010d}_{count:08d}_END"
                                   "\x1b[K").encode("ascii"))
            if control in ready:
                command = parse_command(control.recv(65536), last_generation)
                if command is not None:
                    last_generation = command["generation"]
                    produce(control, driver, command, block, timeout)
    except EOFError:
        pass
    finally:
        control.close()
        if bound:
            os.unlink(address)
        termios.tcsetattr(0, termios.TCSANOW, original)


def terminate(signum, frame):
    raise SystemExit(128 + signum)


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, terminate)
    main()
