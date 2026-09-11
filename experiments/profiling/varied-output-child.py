#!/usr/bin/env python3
"""Raw, file-free tpane benchmark backend. Login-shell argument (-l) is ignored.

Protocol: x -> fixed-position, monotonically numbered ACK; b -> finite ASCII
output then DONE; q -> exit. All other bytes are ignored. The parent supplies
TPANE_BENCH_BYTES. No subprocesses, terminal queries, or clipboard operations.
"""

import os
import termios
import tty


def write_all(data):
    view = memoryview(data)
    while view:
        view = view[os.write(1, view):]


def main():
    tty.setraw(0, termios.TCSANOW)
    pid = os.getpid()
    size = os.get_terminal_size(0)
    total = int(os.environ.get("TPANE_BENCH_BYTES", str(2 * 1024 * 1024)))
    # Exactly 64 ASCII bytes per log line (including CRLF), prepared before READY.
    line = b"2025-01-01T12:34:56 INFO request=012345 status=200 bytes=4096"
    line = line.ljust(62, b" ") + b"\r\n"
    block = b"".join(line.replace(b"012345", f"{i:06d}".encode("ascii")) for i in range(1024))
    write_all((f"\x1b[2J\x1b[1;1HTPB_READY_{pid:010d}_"
               f"{size.columns:03d}x{size.lines:03d}_END").encode("ascii"))
    count = 0
    batches = 0
    while True:
        data = os.read(0, 4096)
        if not data:
            return
        for key in data:
            if key == ord("x"):
                count += 1
                write_all((f"\x1b[1;1HTPB_ACK_{pid:010d}_{count:08d}_END"
                           "\x1b[K").encode("ascii"))
            elif key == ord("b"):
                batches += 1
                write_all(b"\x1b[2J\x1b[1;1H")
                remaining = total
                while remaining:
                    chunk = block[:min(remaining, len(block))]
                    write_all(chunk)
                    remaining -= len(chunk)
                write_all((f"\x1b[2J\x1b[1;1HTPB_DONE_{pid:010d}_"
                           f"{batches:08d}_END").encode("ascii"))
            elif key == ord("q"):
                return


if __name__ == "__main__":
    main()
