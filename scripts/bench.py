#!/usr/bin/env python3
"""Compare native libghostty / tpane / tmux / zellij, normalized to native.

Every variant uses the same direct-C libghostty host and deterministic child.
Native has N terminal instances without a multiplexer. Every tab/terminal produces
output concurrently. CPU includes the host and all multiplexer client/server threads, but
not Python fixture/driver CPU. This is headless processing, not display latency.
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
import select
import shlex
import shutil
import signal
import socket
import statistics
import struct
import subprocess
import sys
import tempfile
import termios
import time
import tty

from benchmark import (TaskCPU, TPANE_NEW_SIBLING, command_output, digest,
                       percentile, process_table)

ROOT = Path(__file__).resolve().parent.parent
VARIANTS = ("native", "tpane", "tmux", "zellij")
READY = re.compile(rb"TPB_READY_(\d{10})_(\d{3})x(\d{3})_END")


def build():
    """Resolve artifacts from this Cargo invocation, never from a stale glob."""
    command = [str(ROOT / "scripts/cargo.sh"), "build", "--release", "--locked",
               "--bin", "tpane", "--message-format=json-render-diagnostics"]
    result = subprocess.run(command, cwd=ROOT, stdout=subprocess.PIPE, text=True, check=True)
    binary = install = None
    for line in result.stdout.splitlines():
        item = json.loads(line)
        if item["reason"] == "compiler-artifact" and item["target"]["name"] == "tpane":
            binary = Path(item["executable"])
        if item["reason"] == "build-script-executed" and "libghostty-vt-sys" in item["package_id"]:
            install = Path(item["out_dir"]) / "ghostty-install"
    if binary is None or install is None:
        raise RuntimeError("Cargo did not report tpane and its native libghostty artifacts")
    host = binary.parent / "bench-host"
    library = install / "lib/libghostty-vt.a"
    subprocess.run(["/usr/bin/clang", "-std=c11", "-O3", "-Wall", "-Wextra", "-Werror",
                    "-I", str(install / "include"), str(ROOT / "scripts/bench-host.c"),
                    str(library), "-o", str(host)], check=True)
    subprocess.run([str(host), "--self-test"], check=True)
    return binary, host, library


class Host:
    """Only completion notifications cross Python; C drains/emulates the PTY."""
    def __init__(self, binary, master, rows, top, errors):
        masters = master if isinstance(master, list) else [master]
        command = ([str(binary), "--multi", str(rows), str(top), *map(str, masters)]
                   if len(masters) > 1 else [str(binary), str(masters[0]), str(rows), str(top)])
        self.process = subprocess.Popen(command, pass_fds=tuple(masters), stdin=subprocess.DEVNULL,
                                        stdout=subprocess.PIPE, stderr=errors)
        self.buffer = b""
        self.master = masters[-1]

    def pop_marker(self, pattern):
        while b"\n" in self.buffer:
            line, self.buffer = self.buffer.split(b"\n", 1)
            match = pattern.fullmatch(line)
            if match:
                return match
        return None

    def read(self):
        data = os.read(self.process.stdout.fileno(), 4096)
        if not data:
            raise RuntimeError("Native host exited before completion")
        self.buffer += data
        if len(self.buffer) > 65536:
            raise RuntimeError("Invalid host notification stream")

    def wait(self, pattern, timeout):
        deadline = time.monotonic() + timeout
        while True:
            match = self.pop_marker(pattern)
            if match:
                return match, time.perf_counter_ns()
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"No parsed-screen marker {pattern.pattern!r}")
            if select.select([self.process.stdout], [], [], remaining)[0]:
                self.read()

    def send(self, data):
        start = time.perf_counter_ns()
        deadline = time.monotonic() + 5
        while True:
            try:
                if os.write(self.master, data) != len(data):
                    raise RuntimeError("Short input write; invalid trial")
                return start
            except BlockingIOError:
                # The C host makes our shared PTY description nonblocking so
                # query replies cannot deadlock startup. Include input stalls.
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not select.select([], [self.master], [], remaining)[1]:
                    raise TimeoutError("PTY input remained blocked")

    def close(self):
        self.process.terminate()
        try:
            self.process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=2)
        self.process.stdout.close()


def descendants(pid):
    found = {pid}
    table = process_table()
    while True:
        new = {child for child, parent in table if parent in found} - found
        if not new:
            return found
        found |= new


def child_records(root):
    return {int(path.name.removeprefix("child-")): tuple(map(int, path.read_text().split()))
            for path in root.glob("child-*") if path.stat().st_size}


def wait_children(root, expected, timeout):
    deadline = time.monotonic() + timeout
    while len(children := child_records(root)) < expected:
        if time.monotonic() > deadline:
            raise TimeoutError(f"Expected {expected} fixture children, got {children}")
        time.sleep(.01)
    return children


def cpu_snapshot(cpu, pids):
    return {str(pid): cpu.snapshot(pid) for pid in sorted(pids)}


def cpu_delta(before, after):
    total = 0
    for pid, sample in before.items():
        for field in ("user_ns", "system_ns"):
            delta = after[pid][field] - sample[field]
            if delta < 0:
                raise RuntimeError("Non-monotonic process CPU counter")
            total += delta
    return total / 1e9


def clock_ns():
    # CPython 3.9 on macOS gives perf_counter/monotonic process-local epochs.
    # POSIX CLOCK_MONOTONIC is shared across the driver and all producers.
    return time.clock_gettime_ns(time.CLOCK_MONOTONIC)


def validate_done(record, sender, root, pids, generation, payload_bytes, start_ns):
    pid = record.get("pid")
    marker = f"TPB_DONE_{pid:010d}_{generation:08d}_END" if type(pid) is int else ""
    if (record.get("kind") != "done" or pid not in pids
            or sender != str(root / f"producer-{pid}.sock")
            or record.get("generation") != generation
            or record.get("payload_bytes") != payload_bytes
            or (record.get("cols"), record.get("rows")) != (100, 28)
            or record.get("dsr") != [1, len(marker) + 1]
            or type(record.get("started_ns")) is not int
            or type(record.get("completed_ns")) is not int
            or not start_ns <= record["started_ns"] <= record["completed_ns"] <= clock_ns()):
        raise RuntimeError(f"Invalid producer completion: {record!r} from {sender!r}")


def output_phase(control, root, children, host, selected_pid, generation, payload_bytes, timeout):
    """Start everyone at one deadline; require every parser barrier + visible DONE."""
    pids = set(children)
    start_ns = clock_ns() + 100_000_000
    command = json.dumps({"generation": generation, "payload_bytes": payload_bytes,
                          "start_ns": start_ns}).encode()
    for pid in sorted(pids):
        # Every command is queued before waiting for any producer to finish.
        if control.sendto(command, str(root / f"producer-{pid}.sock")) != len(command):
            raise RuntimeError("Short producer start command")
    if clock_ns() >= start_ns:
        raise RuntimeError("Missed common producer start deadline; run on an idle machine")
    pattern = re.compile(f"TPB_DONE_{selected_pid:010d}_{generation:08d}_END".encode())
    completed = {}
    visible_ns = None
    deadline = time.monotonic() + timeout + .1
    while len(completed) != len(pids) or visible_ns is None:
        if host.pop_marker(pattern):
            visible_ns = clock_ns()
        if len(completed) == len(pids) and visible_ns is not None:
            break
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(f"Missing parsed completion for producers {sorted(pids - completed.keys())}; "
                               f"visible DONE received={visible_ns is not None}")
        readable, _, _ = select.select([control, host.process.stdout], [], [], remaining)
        if host.process.stdout in readable:
            host.read()
        if control in readable:
            data, sender = control.recvfrom(65536)
            record = json.loads(data)
            validate_done(record, sender, root, pids, generation, payload_bytes, start_ns)
            pid = record["pid"]
            if pid in completed:
                raise RuntimeError(f"Duplicate completion from producer {pid}")
            completed[pid] = record
    # Reject a serial run disguised as concurrent when the payload is too small
    # to overlap across all scheduled producers (especially tiny smoke options).
    if len(pids) > 1 and max(r["started_ns"] for r in completed.values()) >= min(
            r["completed_ns"] for r in completed.values()):
        raise RuntimeError("Producer workloads did not all overlap; increase --mib")
    end_ns = max(visible_ns, max(r["completed_ns"] for r in completed.values()))
    return {"elapsed_seconds": (end_ns - start_ns) / 1e9,
            "payload_bytes": payload_bytes * len(pids), "payload_bytes_per_pane": payload_bytes,
            "output_start_ns": start_ns, "output_completed_ns": end_ns,
            "producer_samples": [completed[pid] for pid in sorted(pids)]}


def run_case(args, binaries, host_binary, variant, panes, trial, cpu):
    with tempfile.TemporaryDirectory(prefix="tpane-bench-", dir="/tmp") as directory:
        root = Path(directory)
        home = root / "home"
        home.mkdir()
        runtime = root / "runtime"
        runtime.mkdir(mode=0o700)
        fixture = root / "child.py"
        source = (ROOT / "scripts/bench-producer.py").read_text()
        fixture.write_text(f"#!{sys.executable}\n" + source.split("\n", 1)[1])
        fixture.chmod(0o700)
        env = {"HOME": str(home), "PWD": str(root), "SHELL": "/bin/sh",
               "PATH": f"{Path(sys.executable).parent}:/usr/bin:/bin",
               "TERM": "xterm-256color", "COLORTERM": "truecolor", "LANG": "en_US.UTF-8",
               "LC_ALL": "en_US.UTF-8", "TMPDIR": str(root),
               "XDG_CONFIG_HOME": str(home / ".config"), "XDG_CACHE_HOME": str(home / ".cache"),
               "XDG_DATA_HOME": str(home / ".local/share"), "XDG_RUNTIME_DIR": str(runtime),
               "ZELLIJ_SOCKET_DIR": str(root / "sockets"),
               "PYTHONNOUSERSITE": "1", "PYTHONDONTWRITEBYTECODE": "1",
               "TPANE_BENCH_READY_DIR": str(root),
               "TPANE_BENCH_CONTROL": str(root / "control.sock"),
               "TPANE_BENCH_TIMEOUT": str(args.timeout)}
        top = 0
        rows = 28
        tmux_socket = root / "tmux.sock"
        session = root.name
        tmux = [str(binaries.get("tmux", "tmux")), "-S", str(tmux_socket)]
        zellij = [str(binaries.get("zellij", "zellij"))]
        if variant == "native":
            command = [str(fixture)]
        elif variant == "tpane":
            command = [str(binaries[variant]), "--shell", str(fixture)]
        elif variant == "tmux":
            config = root / "tmux.conf"
            config.write_text("set -g status off\nset -g history-limit 10000\n"
                              "set -g default-terminal xterm-256color\n")
            command = tmux + ["-f", str(config), "new-session", "-s", "bench",
                              "exec " + shlex.quote(str(fixture))]
        else:
            config = root / "config.kdl"
            config.write_text(f'session_name {json.dumps(session)}\n'
                              'attach_to_session false\nkeybinds clear-defaults=true {}\n'
                              'load_plugins {}\npane_frames false\nauto_layout false\n'
                              'scroll_buffer_size 10000\nsession_serialization false\n'
                              'disable_session_metadata true\nshow_startup_tips false\n'
                              'show_release_notes false\nweb_server false\nweb_sharing "disabled"\n'
                              'on_force_close "quit"\nmouse_mode false\n'
                              'support_kitty_keyboard_protocol false\n')
            layout = root / "layout.kdl"
            layout.write_text("layout {\n" + "".join(
                f'  tab focus={str(index == panes - 1).lower()} {{\n'
                f'    pane command={json.dumps(str(fixture))} borderless=true {{\n'
                f'      close_on_exit true\n    }}\n  }}\n'
                for index in range(panes)) + "}\n")
            command = zellij + ["--config", str(config), "--config-dir", str(root / "config"),
                                "--data-dir", str(root / "data"), "--layout", str(layout)]
        master, slave = pty.openpty()
        process = host = None
        owned = set()
        errors = tempfile.TemporaryFile()
        control = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        extra_masters = []
        extra_processes = []
        try:
            control.bind(env["TPANE_BENCH_CONTROL"])
            control.setblocking(False)
            tty.setraw(slave, termios.TCSANOW)
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, 100, 0, 0))

            def controlling_terminal():
                os.setsid()
                fcntl.ioctl(0, termios.TIOCSCTTY, 0)

            process = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=errors,
                                       cwd=root, env=env, preexec_fn=controlling_terminal)
            if variant == "native":
                # N independent child PTYs, serviced by one C poll loop with N
                # native terminal instances. No extra per-pane host processes.
                for _ in range(1, panes):
                    extra_master, extra_slave = pty.openpty()
                    extra_masters.append(extra_master)
                    try:
                        tty.setraw(extra_slave, termios.TCSANOW)
                        fcntl.ioctl(extra_slave, termios.TIOCSWINSZ,
                                    struct.pack("HHHH", 28, 100, 0, 0))
                        extra_processes.append(subprocess.Popen(
                            command, stdin=extra_slave, stdout=extra_slave, stderr=errors,
                            cwd=root, env=env, preexec_fn=controlling_terminal))
                    finally:
                        os.close(extra_slave)
            host = Host(host_binary, [master, *extra_masters] if extra_masters else master,
                        rows, top, errors)
            os.close(slave)
            slave = None
            if variant == "native":
                ready_by_pid = {}
                for _ in range(panes):
                    record, _ = host.wait(READY, args.timeout)
                    child_pid = int(record.group(1))
                    if child_pid in ready_by_pid:
                        raise RuntimeError(f"Duplicate native READY from {child_pid}")
                    ready_by_pid[child_pid] = record
                ready = ready_by_pid[(extra_processes[-1] if extra_processes else process).pid]
            elif variant in ("tpane", "tmux"):
                ready_pids = set()
                for index in range(panes):
                    if index:
                        if variant == "tpane":
                            host.send(TPANE_NEW_SIBLING)
                        else:
                            subprocess.run(tmux + ["new-window", "-t", "bench",
                                                   "exec " + shlex.quote(str(fixture))],
                                           env=env, check=True, timeout=args.timeout)
                    deadline = time.monotonic() + args.timeout
                    while True:
                        ready, _ = host.wait(READY, max(0, deadline - time.monotonic()))
                        child_pid, cols, body_rows = map(int, ready.groups())
                        if (cols, body_rows) != (100, 28):
                            raise RuntimeError(f"Wrong child viewport: {ready.group()!r}")
                        if child_pid not in ready_pids:
                            ready_pids.add(child_pid)
                            break
            else:
                ready, _ = host.wait(READY, args.timeout)
            expected = panes
            children = wait_children(root, expected, args.timeout)
            if len(children) != expected:
                raise RuntimeError(f"Wrong child count: {children}")
            pid, cols, body_rows = map(int, ready.groups())
            if pid not in children or (cols, body_rows) != (100, 28):
                raise RuntimeError(f"Wrong selected child/viewport: {ready.group()!r}")
            # Track stable process roles, not every startup descendant. Zellij
            # can briefly have launcher/helper descendants that exit during the
            # warmup; retaining their PIDs makes the first CPU snapshot race.
            owned = {process.pid, *(child.pid for child in extra_processes)}
            # Both multiplexers have a daemon; include it even after reparenting.
            if variant == "tmux":
                server = int(subprocess.check_output(tmux + ["display-message", "-p", "#{pid}"],
                                                     env=env, timeout=args.timeout))
                owned.add(server)
            elif variant == "zellij":
                table = command_output(["/bin/ps", "-axo", "pid=,command="])
                servers = {int(line.split(None, 1)[0]) for line in table.splitlines()
                           if "--server" in line and session in line}
                if len(servers) != 1:
                    raise RuntimeError(f"Cannot identify isolated zellij server: {servers}")
                owned.add(servers.pop())
            measured = owned | {host.process.pid}
            # No producer CPU in native, even though it is the launched process.
            if variant == "native":
                measured = {host.process.pid}

            def output(batch):
                return output_phase(control, root, children, host, pid, batch,
                                    args.output_bytes, args.timeout)

            output(1)  # Full unmeasured warmup, including scrollback allocation.
            time.sleep(.1)
            # Required host/client/server exits remain errors. Never suppress
            # ProcessLookupError or change the measured set between snapshots.
            before = cpu_snapshot(cpu, measured)
            workload = output(2)
            after = cpu_snapshot(cpu, measured)
            # Native's C host is the program under test; every other variant is
            # measured together with that same outer host/emulator, so report
            # the multiplexer's own CPU separately from the emulator's.
            host_pid = host.process.pid
            if variant == "native":
                app_cpu, host_cpu = cpu_delta(before, after), 0.0
            else:
                def without(pid, samples):
                    return {key: sample for key, sample in samples.items() if int(key) != pid}
                def only(pid, samples):
                    return {key: sample for key, sample in samples.items() if int(key) == pid}
                app_cpu = cpu_delta(without(host_pid, before), without(host_pid, after))
                host_cpu = cpu_delta(only(host_pid, before), only(host_pid, after))
            latencies = []
            for counter in range(1, args.keys + 1):
                start = host.send(b"x")
                _, end = host.wait(re.compile(f"TPB_ACK_{pid:010d}_{counter:08d}_END".encode()),
                                   args.timeout)
                latencies.append((end - start) / 1e9)
                time.sleep(.01)
            return {"variant": variant, "panes": panes, "trial": trial,
                    "fixture_pids": sorted(children), "measured_pids": sorted(measured),
                    "command": command, "outer_rows": rows, "body_viewport": [100, 28],
                    **workload, "cpu_seconds": cpu_delta(before, after),
                    "app_cpu_seconds": app_cpu, "host_cpu_seconds": host_cpu,
                    "host_pid": host_pid,
                    "key_p95_seconds": percentile(latencies, .95),
                    "key_samples_seconds": latencies, "cpu_before": before, "cpu_after": after}
        except BaseException:
            errors.seek(0)
            print(errors.read().decode(errors="replace")[-8000:], file=sys.stderr)
            raise
        finally:
            try:
                owned.update(child.pid for child in extra_processes)
                stop_case(process, host, owned, root, variant, tmux, zellij, session, env)
            finally:
                if host is not None:
                    host.close()
                if slave is not None:
                    os.close(slave)
                os.close(master)
                for extra_master in extra_masters:
                    os.close(extra_master)
                control.close()
                errors.close()
                for child in extra_processes:
                    child.wait(timeout=3)
                if process is not None:
                    process.wait(timeout=3)


def stop_case(process, host, owned, root, variant, tmux, zellij, session, env):
    # Only private sockets/sessions, never the user's tmux/zellij server.
    if process is None:
        return
    for target in tuple(owned | {process.pid}):
        owned |= descendants(target)
    owned |= child_records(root).keys()
    if variant == "zellij":
        table = command_output(["/bin/ps", "-axo", "pid=,command="])
        for line in table.splitlines():
            if "--server" in line and session in line:
                owned |= descendants(int(line.split(None, 1)[0]))
    if variant in ("tmux", "zellij"):
        stop = tmux + ["kill-server"] if variant == "tmux" else zellij + ["kill-session", session]
        try:
            subprocess.run(stop, env=env, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL, timeout=3)
        except (OSError, subprocess.TimeoutExpired):
            pass
    for target in owned:
        try:
            os.kill(target, signal.SIGTERM)
        except ProcessLookupError:
            pass
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        # A killed macOS PTY child can remain stuck until readers close.
        if host is not None:
            host.close()
        process.kill()
    for target in owned - {process.pid}:
        try:
            os.kill(target, signal.SIGKILL)
        except ProcessLookupError:
            pass
    # The caller closes the master, then reaps the client even on exceptions.


def normalized(samples, panes, variant, field):
    native = {(row["panes"], row["trial"]): row[field]
              for row in samples if row["variant"] == "native"}
    ratios = []
    for row in samples:
        if row["panes"] != panes or row["variant"] != variant:
            continue
        baseline = native[(panes, row["trial"])]
        if not all(math.isfinite(v) and v > 0 for v in (row[field], baseline)):
            raise RuntimeError(f"Unmeasurable {field}; use a larger workload/more samples")
        ratios.append(row[field] / baseline)
    return statistics.median(ratios)


LABELS = {"native": "Native", "tpane": "Us", "tmux": "tmux", "zellij": "Zellij"}


def summary(report):
    print("\nAll costs relative to native (1.00×); higher = worse.")
    print("All tabs produce concurrently; native uses the same number of terminal instances.")
    variants = report["options"]["variants"]
    for panes in report["options"]["panes"]:
        print(f"\n### {panes} {'tab' if panes == 1 else 'tabs'}\n")
        print("| Metric | " + " | ".join(LABELS[v] for v in variants) + " |")
        print("|---|" + "---:|" * len(variants))
        for label, field in (("Output time", "elapsed_seconds"),
                             ("CPU cost", "cpu_seconds"),
                             ("Key p95 (quiet)", "key_p95_seconds")):
            values = [normalized(report["samples"], panes, variant, field) for variant in variants]
            print(f"| {label} | " + " | ".join(f"{v:.2f}×" for v in values) + " |")


def absolute(report):
    """Median absolute cost per pane count; used when no native baseline was requested."""
    print("\nAbsolute results (native absent from --variants; no ratios).")
    print("All tabs produce concurrently; every tab must parse its full workload.")
    variants = report["options"]["variants"]
    metrics = (
        ("Output time", lambda r: r["elapsed_seconds"], "{:.3f} s"),
        ("Aggregate throughput", lambda r: r["payload_bytes"] / r["elapsed_seconds"] / 1048576,
         "{:.1f} MiB/s"),
        ("Per-pane throughput",
         lambda r: r["payload_bytes_per_pane"] / r["elapsed_seconds"] / 1048576, "{:.2f} MiB/s"),
        ("CPU cost (total)", lambda r: r["cpu_seconds"], "{:.3f} s"),
        ("CPU cost (app)", lambda r: r["app_cpu_seconds"], "{:.3f} s"),
        ("CPU cost (outer host)", lambda r: r["host_cpu_seconds"], "{:.3f} s"),
        ("App CPU per MiB",
         lambda r: r["app_cpu_seconds"] * 1000 / (r["payload_bytes"] / 1048576), "{:.2f} ms/MiB"),
        ("App CPU / output time",
         lambda r: 100 * r["app_cpu_seconds"] / r["elapsed_seconds"], "{:.1f}% of one core"),
        ("Key p95 (quiet)", lambda r: r["key_p95_seconds"] * 1000, "{:.3f} ms"),
    )
    for panes in report["options"]["panes"]:
        print(f"\n### {panes} {'tab' if panes == 1 else 'tabs'}\n")
        print("| Metric | " + " | ".join(LABELS[v] for v in variants) + " |")
        print("|---|" + "---:|" * len(variants))
        for label, value_of, fmt in metrics:
            values = []
            for variant in variants:
                samples = [row for row in report["samples"]
                           if row["panes"] == panes and row["variant"] == variant]
                if not samples:
                    raise RuntimeError(f"No samples for {variant} at {panes} tabs")
                values.append(statistics.median(value_of(row) for row in samples))
            print(f"| {label} | " + " | ".join(fmt.format(value) for value in values) + " |")


def arguments():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--panes", default="1,8,24", help="total tabs; every tab produces concurrently")
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--mib", type=float, default=32, help="changing-log payload PER TAB per output phase")
    parser.add_argument("--keys", type=int, default=30)
    parser.add_argument("--timeout", type=float, default=60)
    parser.add_argument("--variants", default=",".join(VARIANTS),
                        help="comma-separated implementations; absolute results unless native is included")
    parser.add_argument("--output", type=Path, help="raw JSON (default: unique .tools/bench/run-*.json)")
    args = parser.parse_args()
    try:
        args.panes = [int(value) for value in args.panes.split(",")]
    except ValueError:
        parser.error("--panes must be comma-separated positive integers")
    args.variants = args.variants.split(",")
    if not args.panes or min(args.panes) < 1 or len(set(args.panes)) != len(args.panes):
        parser.error("--panes must contain unique positive integers")
    if (not args.variants or len(set(args.variants)) != len(args.variants)
            or any(v not in VARIANTS for v in args.variants)):
        parser.error(f"--variants must be unique names from {VARIANTS}")
    args.variants = [v for v in VARIANTS if v in args.variants]
    if (min(args.trials, args.keys) < 1 or args.keys >= 100000000
            or not all(math.isfinite(v) and v > 0 for v in (args.mib, args.timeout))):
        parser.error("trials/keys/mib/timeout must be positive and finite; keys <100000000")
    if not math.isfinite(args.mib * 1048576) or args.mib * 1048576 > sys.maxsize:
        parser.error("--mib is too large")
    args.output_bytes = int(args.mib * 1048576)
    if args.output_bytes < 1:
        parser.error("--mib must specify at least one byte")
    if any(c.isspace() for c in sys.executable):
        parser.error("Python executable path cannot contain whitespace (fixture shebang)")
    if args.output is None:
        args.output = ROOT / ".tools/bench" / (datetime.datetime.now().strftime("run-%Y%m%d-%H%M%S-%f") + ".json")
    args.output = args.output.resolve()
    if args.output.exists():
        parser.error("--output must be a fresh path")
    return args


def main():
    args = arguments()
    if sys.platform != "darwin":
        raise SystemExit("This benchmark currently requires macOS (same as tpane)")
    binaries = {}
    for variant in ("tmux", "zellij"):
        if variant in args.variants:
            path = shutil.which(variant)
            if path is None:
                raise SystemExit(f"Missing {variant}. Install it (brew install {variant}) and rerun just bench.")
            binaries[variant] = Path(path).resolve()
    with open(f"/tmp/tpane-benchmark-{os.getuid()}.lock", "a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise SystemExit("Another benchmark is running; run benchmarks serially")
        binaries["tpane"], host, library = build()
        cpu = TaskCPU()
        artifacts = {**binaries, "native_host": host, "native_library": library,
                     "harness": Path(__file__), "host_source": ROOT / "scripts/bench-host.c",
                     "fixture": ROOT / "scripts/bench-producer.py"}
        hashes = {name: {"path": str(path), "sha256": digest(path)} for name, path in artifacts.items()}
        report = {"schema_version": 2, "status": "running",
                  "options": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
                  "environment": {"platform": platform.platform(), "python": sys.version,
                                  "cpu": command_output(["/usr/sbin/sysctl", "-n", "machdep.cpu.brand_string"]),
                                  "cpu_counter_validation": cpu.validation, "artifacts": hashes,
                                  "versions": {v: command_output([str(binaries[v]), "-V" if v == "tmux" else "--version"])
                                               for v in args.variants if v != "native"}},
                  "method": {"native": "N terminal instances/PTYS in one C poll loop using direct libghostty API; same static library as tpane; no multiplexer",
                             "host": "Same C PTY reader/parser for every variant; no GUI/GPU",
                             "cpu": "host + multiplexer client/server processes, all threads; excludes Python driver/children",
                             "workload": "All N producers start at a shared monotonic deadline, each emits --mib; their production intervals must overlap",
                             "completion": "Every child's ordered cursor-query response after DONE proves its terminal parsed all output, including hidden tabs; selected DONE also verified in host cells with synchronized output closed",
                             "ratios": "median of per-trial cost/native ratios at each pane count; higher is worse",
                             "absolute": "median of per-trial absolute output/CPU/latency when native is not in --variants; app CPU excludes the shared outer C host",
                             "cpu_split": "app is the multiplexer processes; outer host is the shared C PTY reader/parser; native reports its C host as app",
                             "app_cpu": "tpane/tmux/zellij client+server processes only, excluding the outer C host; native: its whole C host",
                             "host_cpu": "the shared outer C host/emulator process; zero for native",
                             "throughput": "aggregate payload MiB over the elapsed output phase, all tabs; per-pane divides by pane count",
                             "app_cpu_per_mib": "app CPU milliseconds per MiB of aggregate payload",
                             "app_cpu_fraction": "app CPU seconds over elapsed output seconds, as percent of one core",
                             "absolute_units": {"output": "s", "throughput": "MiB/s", "cpu": "s", "key_p95": "ms"},
                             "warmup": "one full output workload per fresh case, excluded from measurements",
                             "geometry": "100x28 host and child viewport for every variant and pane count; no persistent bars",
                             "tpane_navigation": "Ctrl+Space (NUL), n creates/selects each sibling and dismisses explorer; final leaf remains selected",
                             "limitations": ["finite concurrent output, not an indefinitely sustained flood",
                                             "native emulates N terminal instances, no graphical windows",
                                             "output frames may coalesce; no intermediate-frame equivalence",
                                             "wall time includes producer, PTY and driver scheduling",
                                             "key p95 is quiet-input latency, not during flooding",
                                             "no driver subtraction; no statistical significance claim"]},
                  "samples": []}
        args.output.parent.mkdir(parents=True, exist_ok=True)

        def save():
            args.output.write_text(json.dumps(report, indent=2, allow_nan=False) + "\n")

        def interrupted(signum, frame):
            raise KeyboardInterrupt(f"signal {signum}")

        previous = signal.signal(signal.SIGTERM, interrupted)
        print("Run on an otherwise idle machine. Building and warmups are not timed.", file=sys.stderr)
        try:
            for trial in range(1, args.trials + 1):
                for index, panes in enumerate(args.panes):
                    shift = (trial - 1 + index) % len(args.variants)
                    order = args.variants[shift:] + args.variants[:shift]
                    for variant in order:
                        print(f"trial {trial}/{args.trials}: {variant}, tabs={panes}", file=sys.stderr)
                        report["active_case"] = {"variant": variant, "panes": panes, "trial": trial}
                        report["samples"].append(run_case(args, binaries, host, variant, panes, trial, cpu))
                        del report["active_case"]
                        save()
            for name, path in artifacts.items():
                if digest(path) != hashes[name]["sha256"]:
                    raise RuntimeError(f"{name} changed during benchmark; results invalid")
            # Validate every denominator before marking a report complete.
            if "native" in args.variants:
                for panes in args.panes:
                    for variant in args.variants:
                        for field in ("elapsed_seconds", "cpu_seconds", "key_p95_seconds"):
                            normalized(report["samples"], panes, variant, field)
            else:
                for row in report["samples"]:
                    for field in ("elapsed_seconds", "app_cpu_seconds", "host_cpu_seconds",
                                  "key_p95_seconds"):
                        value = row[field]
                        if not math.isfinite(value) or value < 0:
                            raise RuntimeError(f"Unmeasurable {field} in absolute mode")
            report["status"] = "complete"
        except BaseException as error:
            report["status"] = "failed"
            report["error"] = f"{type(error).__name__}: {error}"
            save()
            raise
        finally:
            signal.signal(signal.SIGTERM, previous)
        save()
        if "native" in args.variants:
            summary(report)
        else:
            absolute(report)
        print(f"\nRaw results: {args.output}")


if __name__ == "__main__":
    main()
