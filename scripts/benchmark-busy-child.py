#!/usr/bin/env python3
"""Event-driven raw PTY fixture; ignores shell -l. No config/clipboard access.

SIGUSR1 starts/resets production; SIGUSR2 stops, atomically saves accepted-write
stats in cwd, then queues DONE after all accepted output. Unwritten payload is
discarded on stop/reset. x -> ACK; q -> exit. No polling or subprocesses.
"""

import json
import os
from pathlib import Path
import select
import signal
import termios
import time
import tty


def main():
    tty.setraw(0, termios.TCSANOW)
    os.set_blocking(1, False)
    wake_r, wake_w = os.pipe()
    os.set_blocking(wake_r, False)
    os.set_blocking(wake_w, False)
    signal.set_wakeup_fd(wake_w)
    for sig in (signal.SIGUSR1, signal.SIGUSR2):
        signal.signal(sig, lambda *_: None)
    pid = os.getpid()
    size = os.get_terminal_size(0)
    control = (f"\x1b[2J\x1b[1;1HTPB_READY_{pid:010d}_"
               f"{size.columns:03d}x{size.lines:03d}_END").encode()
    line = f"TPB_BUSY_{pid:010d}_ ASCII continuous producer".encode().ljust(62, b" ") + b"\r\n"
    assert len(line) == 64
    block = line * 256  # At most 16 KiB pending payload, never an unbounded queue.
    pending = b""
    producing = False
    count = generation = accepted = 0
    started = cpu_started = 0
    while True:
        readable, writable, _ = select.select(
            [wake_r] + ([0] if len(control) < 1024 else []),
            [1] if control or pending or producing else [], [])
        if wake_r in readable:
            for sig in os.read(wake_r, 4096):
                if sig == signal.SIGUSR1:
                    generation += 1
                    accepted = 0
                    pending = b""
                    started, cpu_started = time.perf_counter_ns(), time.process_time_ns()
                    producing = True
                elif sig == signal.SIGUSR2 and producing:
                    stopped, cpu_stopped = time.perf_counter_ns(), time.process_time_ns()
                    producing = False
                    stats = {"pid": pid, "generation": generation,
                             "accepted_bytes": accepted, "unwritten_discarded_bytes": len(pending),
                             "start_ns": started, "stop_ns": stopped,
                             "window_seconds": (stopped - started) / 1e9,
                             "backend_cpu_seconds": (cpu_stopped - cpu_started) / 1e9}
                    pending = b""
                    path = Path(f"busy-{pid}.json")
                    temp = path.with_suffix(".tmp")
                    temp.write_text(json.dumps(stats))
                    temp.replace(path)
                    control += (f"\x1b[2J\x1b[1;1HTPB_DONE_{pid:010d}_"
                                f"{generation:08d}_END").encode()
        if 0 in readable:
            data = os.read(0, 64)
            if not data or b"q" in data:
                return
            for key in data:
                if key == ord("x"):
                    count += 1
                    control += (f"\x1b[1;1HTPB_ACK_{pid:010d}_{count:08d}_END\x1b[K").encode()
        if 1 in writable:
            payload = not control
            if payload and not pending and producing:
                pending = block
            data = pending if payload else control
            if data:
                try:
                    written = os.write(1, data)
                except BlockingIOError:
                    continue
                if payload:
                    accepted += written  # Only successful write bytes, excluding markers.
                    pending = pending[written:]
                else:
                    control = control[written:]


if __name__ == "__main__":
    main()
