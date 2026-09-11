#!/usr/bin/env python3
"""Real-program workload benchmarks for tpane.

The synthetic `just bench` flood measures maximum throughput. This harness
measures the patterns real terminal programs actually produce: full-screen TUI
repaints, keystroke-driven redraws, incremental log appends, partial-row
progress updates, color/style churn, wide and grapheme text, and nested
pagers/multiplexers. Every scenario runs a real program in one pane over a raw
PTY host (no outer emulator) with pinned fixtures and scripted input.

Reported per scenario: tpane task and per-thread CPU (libproc), host bytes,
completed frames, bytes per frame, per-key frame latency, and wall time.

Usage:
  python3 scripts/benchmark-programs.py --binary target/release/tpane \\
      --trials 3 --output programs.json
  python3 scripts/benchmark-programs.py --binary a/tpane --binary b/tpane \\
      --scenarios nvim-scroll,less-page --trials 3

Programs are external and unpinned: record versions and compare binaries only
on the same machine, same program versions, and interleaved trials. Diagnostic,
not part of the supported benchmark gate. Run serially on an idle machine.
"""

import argparse
import ctypes
import fcntl
import json
import os
import pty
import select
import shutil
import signal
import statistics
import struct
import subprocess
import sys
import tempfile
import termios
import time
import tty
from dataclasses import dataclass
from pathlib import Path

FRAME_BEGIN = b"\x1b[?2026h"
FRAME_END = b"\x1b[?2026l"
PATH = ":".join(filter(None, [
    str(Path.home() / ".cargo" / "bin"),
    "/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin",
    os.environ.get("PATH", "")]))

_libproc = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
_libproc.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64,
                                  ctypes.c_void_p, ctypes.c_int]
_libproc.proc_pidinfo.restype = ctypes.c_int


class Timebase(ctypes.Structure):
    _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]


_timebase = Timebase()
ctypes.CDLL("/usr/lib/libSystem.B.dylib").mach_timebase_info(ctypes.byref(_timebase))


class ThreadInfo(ctypes.Structure):
    _fields_ = [(name, ctypes.c_uint64) for name in ("user", "system")] + [
        (name, ctypes.c_int32)
        for name in ("cpu_usage", "policy", "run_state", "flags", "sleep_time",
                     "curpri", "priority", "maxpriority")
    ] + [("name", ctypes.c_char * 64)]


class TaskInfo(ctypes.Structure):
    _fields_ = [(name, ctypes.c_uint64) for name in (
        "pti_virtual_size", "pti_resident_size", "pti_total_user",
        "pti_total_system", "pti_threads_user", "pti_threads_system")] + [
        (name, ctypes.c_int32) for name in (
            "pti_policy", "pti_faults", "pti_pageins", "pti_cow_faults",
            "pti_messages_sent", "pti_messages_received", "pti_syscalls_mach",
            "pti_syscalls_unix", "pti_csw", "pti_threadnum", "pti_numrunning",
            "pti_priority")]


def task_cpu(pid):
    """Cumulative task user+system seconds (task totals are mach ticks)."""
    info = TaskInfo()
    size = ctypes.sizeof(info)
    if _libproc.proc_pidinfo(pid, 4, 0, ctypes.byref(info), size) != size:
        raise OSError(ctypes.get_errno(), f"proc_pidinfo({pid})")
    ticks = info.pti_total_user + info.pti_total_system
    return ticks * _timebase.numer / _timebase.denom / 1e9


def thread_cpu(pid):
    """Cumulative per-thread user+system seconds (thread counters are ns)."""
    handles = (ctypes.c_uint64 * 1024)()
    count = _libproc.proc_pidinfo(pid, 6, 0, handles, ctypes.sizeof(handles))
    if count <= 0 or count % 8:
        return {}
    totals = {}
    for index in range(count // 8):
        info = ThreadInfo()
        size = ctypes.sizeof(info)
        if _libproc.proc_pidinfo(pid, 5, handles[index], ctypes.byref(info), size) != size:
            continue
        name = info.name.decode(errors="replace") or f"thread{index}"
        totals[name] = (info.user + info.system) / 1e9
    return totals


def percentile(values, fraction):
    if not values:
        return None
    ordered = sorted(values)
    index = min(len(ordered) - 1, int(fraction * len(ordered)))
    return ordered[index]


@dataclass
class Step:
    """One scripted phase: keys with think time, a timed drain, or quiet wait."""

    kind: str  # "key", "time", "quiet"
    data: bytes = b""
    count: int = 1
    think_ms: float = 5.0
    seconds: float = 1.0
    quiet_seconds: float = 0.3
    latency: bool = True


@dataclass
class Workload:
    name: str
    command: object  # (root: Path) -> str
    steps: list
    settle: float = 2.0
    quit: bytes = b""
    background: object = None  # (root: Path) -> subprocess.Popen
    units: str = "keys"
    note: str = ""
    timeout: float = 45.0


class Stream:
    def __init__(self, fd, process):
        self.fd = fd
        self.process = process
        self.bytes = 0
        self.frames = 0
        self.carry = b""

    def _account(self, data):
        self.bytes += len(data)
        joined = self.carry + data
        self.frames += joined.count(FRAME_BEGIN)
        self.carry = joined[-len(FRAME_BEGIN) + 1:]

    def drain(self, seconds, quiet_only=False):
        deadline = time.monotonic() + seconds
        while True:
            if self.process.poll() is not None and not select.select([self.fd], [], [], 0)[0]:
                return
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return
            if not select.select([self.fd], [], [], min(remaining, 0.05))[0]:
                if quiet_only:
                    return
                continue
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                return
            if not data:
                return
            self._account(data)

    def send(self, data):
        try:
            os.write(self.fd, data)
        except OSError:
            pass

    def wait_frame_end(self, timeout):
        deadline = time.monotonic() + timeout
        window = self.carry
        while time.monotonic() < deadline:
            remaining = deadline - time.monotonic()
            if not select.select([self.fd], [], [], remaining)[0]:
                break
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                break
            if not data:
                break
            window += data
            self._account(data)
            if FRAME_END in window:
                return True
        return False


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

def log_block(mib):
    line = b"2025-01-01T12:34:56 INFO request=012345 status=200 bytes=4096"
    line = line.ljust(62, b" ") + b"\r\n"
    block = b"".join(line.replace(b"012345", f"{i:06d}".encode()) for i in range(1024))
    return block * max(1, (mib * 1024 * 1024) // len(block) + 1)


def source_lines(count):
    return "".join(
        f"fn item_{index:05d}(value: u32, scale: u32) -> u32 {{ value * scale + {index} }}\n"
        for index in range(count))


def make_repo(root):
    repo = root / "repo"
    repo.mkdir()
    env = dict(os.environ, HOME=str(root / "home"), GIT_CONFIG_GLOBAL="/dev/null",
               GIT_CONFIG_SYSTEM="/dev/null", GIT_AUTHOR_NAME="Bench",
               GIT_AUTHOR_EMAIL="bench@example.com", GIT_COMMITTER_NAME="Bench",
               GIT_COMMITTER_EMAIL="bench@example.com")
    subprocess.run(["git", "init", "-q", "-b", "main"], cwd=repo, env=env, check=True)
    for commit in range(80):
        for file in range(3):
            path = repo / f"src_{file}" / f"file_{commit:03d}.rs"
            path.parent.mkdir(exist_ok=True)
            path.write_text(source_lines(160 + commit))
        subprocess.run(["git", "add", "-A"], cwd=repo, env=env, check=True)
        subprocess.run(["git", "commit", "-q", "-m", f"commit {commit}: change {commit}"],
                       cwd=repo, env=env, check=True)
    untracked = repo / "notes"
    untracked.mkdir()
    for index in range(200):
        (untracked / f"note_{index:03d}.md").write_text(f"# note {index}\n" * 20)
    return repo, env


def make_tree(root):
    tree = root / "tree"
    tree.mkdir()
    for index in range(400):
        path = tree / f"mod_{index // 20:02d}" / f"file_{index:03d}.rs"
        path.parent.mkdir(exist_ok=True)
        path.write_text("\n".join(
            f"fn helper_{line}(x: u32) -> u32 {{ x + {line} }}" for line in range(120)) + "\n")
    return tree


def unicode_text(count):
    samples = [
        "界面渲染路径 test 中文宽度",
        "e\u0301 combining accents a\u0300a\u0301",
        "\U0001f469\u200d\U0001f4bb \U0001f680 \U0001f1ef\U0001f1f5",
        "\u250c\u2500\u252c\u2500\u2510 box drawing",
        "\u6587\u5b57\u3068\u304b\u306a\u30ab\u30ca",
    ]
    return "".join(f"{index:06d} {samples[index % len(samples)]}\n" for index in range(count))


def pane_script(root, name, body):
    """Wrap a pane command; one-shot programs must not close the pane."""
    path = root / f"{name}.sh"
    path.write_text(f"#!/bin/sh\n(\n{body}\n)\nexec sleep 300\n")
    path.chmod(0o700)
    return str(path)


# ---------------------------------------------------------------------------
# Scenarios
# ---------------------------------------------------------------------------

SCENARIOS = {}


def scenario(workload):
    SCENARIOS[workload.name] = workload


scenario(Workload(
    name="cat-flood",
    note="real cat of 32 MiB of logs: parsing/scroll throughput",
    units="seconds",
    command=lambda root: pane_script(root, "cat", f'exec cat "{root / "big.txt"}"'),
    steps=[Step("quiet", seconds=20.0, quiet_seconds=0.3)],
    settle=0.5,
))

scenario(Workload(
    name="seq-flood",
    note="real seq of 3M numbers: number rendering and line flood",
    units="seconds",
    command=lambda root: pane_script(root, "seq", "exec seq 1 3000000"),
    steps=[Step("quiet", seconds=20.0, quiet_seconds=0.3)],
    settle=0.5,
))

scenario(Workload(
    name="less-page",
    note="less paging 32 MiB: full-screen repaint per page",
    command=lambda root: pane_script(root, "less", f'exec less -R "{root / "big.txt"}"'),
    steps=[Step("key", b"\x1b[6~", count=60), Step("key", b"\x1b[5~", count=60)],
    quit=b"q",
))

scenario(Workload(
    name="nvim-scroll",
    note="nvim single-line scrolling: keystroke-driven partial repaints",
    command=lambda root: pane_script(
        root, "nvim", f'exec nvim --clean -u NONE -c "set noswapfile" "{root / "work.rs"}"'),
    steps=[Step("key", b"\x05", count=300, think_ms=8.0),
           Step("key", b"\x19", count=300, think_ms=8.0)],
    quit=b":q!\r",
))

scenario(Workload(
    name="nvim-typing",
    note="nvim insert-mode typing: per-keystroke latency and status-line updates",
    command=lambda root: pane_script(
        root, "nvim-type", f'exec nvim --clean -u NONE -c "set noswapfile" "{root / "work.rs"}"'),
    steps=[Step("key", b"i", count=1), Step("key", b"the quick brown fox ", count=20,
                                            think_ms=5.0), Step("key", b"\x1b", count=1)],
    quit=b":q!\r",
))

scenario(Workload(
    name="lazygit-nav",
    note="lazygit navigation and diff views: heavy full-screen TUI repaints",
    command=lambda root: pane_script(
        root, "lazygit",
        f'cd "{root / "repo"}" && exec lazygit'),
    steps=[Step("key", b"\x1b[B", count=150, latency=False, think_ms=10.0),
           Step("key", b"\x1b[A", count=150, latency=False, think_ms=10.0),
           Step("key", b"\t", count=30, latency=False, think_ms=15.0),
           Step("key", b"\r", count=30, latency=False, think_ms=30.0),
           Step("key", b"\x1b", count=30, latency=False, think_ms=30.0),
           Step("time", seconds=2.0)],
    quit=b"q",
    settle=4.0,
))

scenario(Workload(
    name="htop-nav",
    note="htop process-list navigation: periodic and keystroke full repaints",
    command=lambda root: pane_script(root, "htop", "exec htop -d 10"),
    steps=[Step("key", b"\x1b[B", count=30, latency=False, think_ms=10.0),
           Step("key", b"\x1b[A", count=30, latency=False, think_ms=10.0),
           Step("time", seconds=8.0)],
    quit=b"q",
))

scenario(Workload(
    name="fzf-query",
    note="fzf incremental filtering: whole-screen redraw per typed character",
    command=lambda root: pane_script(
        root, "fzf", f'exec fzf --no-mouse --no-height < "{root / "list.txt"}"'),
    steps=[Step("key", b"file_0001", count=1, think_ms=20.0),
           Step("key", b"2", count=60, think_ms=15.0)],
    quit=b"\x03",
))

scenario(Workload(
    name="tail-f",
    note="tail -f with a writer appending a line every 3 ms: incremental rows",
    units="seconds",
    background=lambda root: subprocess.Popen([
        sys.executable, "-c",
        "import sys,time\n"
        f"path=open(sys.argv[1],'a',buffering=1)\n"
        "line='2025-01-01T12:34:56 INFO request=%06d status=200 bytes=4096\\n'\n"
        "for i in range(2000):\n"
        "    path.write(line % i)\n"
        "    time.sleep(0.003)\n",
        str(root / "tail.log")], cwd=root),
    command=lambda root: pane_script(root, "tail", f'exec tail -f "{root / "tail.log"}"'),
    steps=[Step("time", seconds=6.0)],
))

scenario(Workload(
    name="progress-bar",
    note="pattern generator: \\r progress updates with erase-line (curl/tqdm shape)",
    units="seconds",
    command=lambda root: pane_script(
        root, "progress", f'exec "{sys.executable}" "{root / "progress.py"}"'),
    steps=[Step("time", seconds=7.0)],
))

scenario(Workload(
    name="style-churn",
    note="grep --color over a tree: SGR/style-change-heavy flood",
    units="seconds",
    command=lambda root: pane_script(
        root, "grep",
        f'exec grep --color=always -r "fn helper_" "{root / "tree"}"'),
    steps=[Step("quiet", seconds=20.0, quiet_seconds=0.3)],
    settle=0.5,
))

scenario(Workload(
    name="wide-emoji",
    note="cat of CJK/emoji/combining text: wide, spacer, and grapheme rows",
    units="seconds",
    command=lambda root: pane_script(root, "unicode", f'exec cat "{root / "unicode.txt"}"'),
    steps=[Step("quiet", seconds=20.0, quiet_seconds=0.3)],
    settle=0.5,
))

scenario(Workload(
    name="git-log-flood",
    note="git log --graph -p --color: colored graph and patch output",
    units="seconds",
    command=lambda root: pane_script(
        root, "gitlog",
        f'cd "{root / "repo"}" && exec git --no-pager log --graph --color=always -p'),
    steps=[Step("quiet", seconds=20.0, quiet_seconds=0.3)],
    settle=0.5,
))

scenario(Workload(
    name="tmux-nested",
    note="tmux inside a tpane pane: nested multiplexer redraws and windows",
    command=lambda root: pane_script(root, "tmux", "exec tmux -u -f /dev/null new -A -s bench"),
    steps=[Step("key", b"\x02c", count=20, latency=False, think_ms=20.0),
           Step("key", b"\x02%", count=10, latency=False, think_ms=20.0),
           Step("key", b"\x02o", count=20, latency=False, think_ms=20.0),
           Step("time", seconds=2.0)],
    quit=b"\x02d",
    settle=2.0,
))


def prepare(workload, root):
    """Create the fixtures this scenario needs; callable command builds the rest."""
    name = workload.name
    if name in ("cat-flood", "less-page"):
        (root / "big.txt").write_bytes(log_block(32))
    if name.startswith("nvim"):
        (root / "work.rs").write_text(source_lines(20000) if name == "nvim-scroll" else "")
    if name in ("lazygit-nav", "git-log-flood"):
        make_repo(root)
    if name == "fzf-query":
        (root / "list.txt").write_text(
            "".join(f"file_{index:06d}.txt\n" for index in range(500000)))
    if name == "tail-f":
        (root / "tail.log").write_text("")
    if name == "style-churn":
        make_tree(root)
    if name == "progress-bar":
        (root / "progress.py").write_text(
            "import sys, time\n"
            "for index in range(3000):\n"
            "    bar = '#' * (index % 40) + ' ' * (40 - index % 40)\n"
            "    sys.stdout.write(f'\\r 42% |{bar}| {index}/3000\\x1b[K')\n"
            "    sys.stdout.flush()\n"
            "    time.sleep(0.002)\n")
    if name == "wide-emoji":
        (root / "unicode.txt").write_text(unicode_text(60000))
    (root / "home").mkdir(exist_ok=True)


def run_workload(workload, binary):
    result = {"scenario": workload.name, "binary": str(binary), "note": workload.note}
    with tempfile.TemporaryDirectory(prefix=f"tpane-prog-{workload.name}-") as directory:
        root = Path(directory)
        prepare_started = time.monotonic()
        prepare(workload, root)
        result["prepare_s"] = time.monotonic() - prepare_started
        command = workload.command(root)
        env = {
            "HOME": str(root / "home"), "PWD": str(root), "SHELL": command,
            "PATH": PATH, "TERM": "xterm-256color", "COLORTERM": "truecolor",
            "LANG": "C", "LC_ALL": "C", "TMPDIR": str(root),
            "XDG_CONFIG_HOME": str(root / "home" / ".config"),
            "XDG_CACHE_HOME": str(root / "home" / ".cache"),
            "XDG_DATA_HOME": str(root / "home" / ".local" / "share"),
            "XDG_STATE_HOME": str(root / "home" / ".local" / "state"),
            "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_SYSTEM": "/dev/null",
            "GIT_TERMINAL_PROMPT": "0", "EDITOR": "true",
        }
        background = workload.background(root) if workload.background else None
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 28, 100, 0, 0))
        tty.setraw(slave, termios.TCSANOW)

        def controlling_terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        process = subprocess.Popen(
            [str(binary.resolve()), "--shell", command],
            stdin=slave, stdout=slave, stderr=slave, cwd=root, env=env,
            preexec_fn=controlling_terminal)
        os.close(slave)
        stream = Stream(master, process)
        run_started = time.monotonic()
        startup_started = time.monotonic()
        try:
            stream.drain(workload.settle, quiet_only=True)
            result["startup_s"] = time.monotonic() - startup_started
            if process.poll() is not None:
                raise RuntimeError("tpane exited before measurement")
            started = time.monotonic()
            before = task_cpu(process.pid)
            before_threads = thread_cpu(process.pid)
            latencies = []
            keys = 0
            deadline = time.monotonic() + workload.timeout
            for step in workload.steps:
                if time.monotonic() >= deadline:
                    result["timed_out"] = True
                    break
                if step.kind == "key":
                    for _ in range(step.count):
                        if time.monotonic() >= deadline:
                            result["timed_out"] = True
                            break
                        stream.drain(0.005, quiet_only=True)
                        start = time.monotonic()
                        stream.send(step.data)
                        if step.latency:
                            remaining = max(0.05, deadline - time.monotonic())
                            if stream.wait_frame_end(min(2.0, remaining)):
                                latencies.append(time.monotonic() - start)
                        keys += 1
                        if step.think_ms:
                            time.sleep(step.think_ms / 1000.0)
                elif step.kind == "time":
                    stream.drain(min(step.seconds, max(0.0, deadline - time.monotonic())))
                elif step.kind == "quiet":
                    quiet_deadline = min(time.monotonic() + step.seconds, deadline)
                    while time.monotonic() < quiet_deadline:
                        size = stream.bytes
                        stream.drain(step.quiet_seconds)
                        if stream.bytes == size:
                            break
            if workload.quit:
                stream.send(workload.quit)
                stream.drain(0.5, quiet_only=True)
            after = task_cpu(process.pid)
            after_threads = thread_cpu(process.pid)
            elapsed = time.monotonic() - started
            result.update({
                "keys": keys,
                "seconds": elapsed,
                "cpu_seconds": after - before,
                "cpu_ms_per_key": (after - before) * 1000 / keys if keys else None,
                "cpu_ms_per_second": (after - before) * 1000 / elapsed,
                "bytes": stream.bytes,
                "frames": stream.frames,
                "bytes_per_frame": stream.bytes / stream.frames if stream.frames else None,
                "latency_mean_ms": (sum(latencies) / len(latencies) * 1000) if latencies else None,
                "latency_p95_ms": percentile(latencies, 0.95) * 1000 if latencies else None,
                "threads": {
                    name: round(after_threads.get(name, 0) - before_threads.get(name, 0), 4)
                    for name in after_threads
                },
            })
        finally:
            signal.setitimer(signal.ITIMER_REAL, 0)
            teardown_started = time.monotonic()
            try:
                os.close(master)
            except OSError:
                pass
            if process.poll() is None:
                try:
                    os.killpg(os.getpgid(process.pid), signal.SIGKILL)
                except OSError:
                    pass
                process.kill()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    result["teardown_hung"] = True
            if background and background.poll() is None:
                background.kill()
                try:
                    background.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    result["teardown_hung"] = True
            result["teardown_s"] = time.monotonic() - teardown_started
            result["wall_s"] = time.monotonic() - run_started
    return result


TOOLS = {
    "nvim": ["nvim", "--version"], "lazygit": ["lazygit", "--version"],
    "htop": ["htop", "--version"], "fzf": ["fzf", "--version"],
    "tmux": ["tmux", "-V"], "git": ["git", "--version"], "less": ["less", "--version"],
}


def tool_versions():
    versions = {}
    for name, command in TOOLS.items():
        if not shutil.which(command[0]):
            continue
        try:
            done = subprocess.run(command, capture_output=True, text=True, timeout=10)
            versions[name] = (done.stdout or done.stderr).splitlines()[0].strip()
        except Exception as error:  # diagnosis only
            versions[name] = f"unknown: {error}"
    return versions


def hard_timeout(signum, frame):
    raise TimeoutError("scenario hard timeout")


def report(samples):
    by_scenario = {}
    for record in samples:
        by_scenario.setdefault(record["scenario"], []).append(record)
    metrics = (
        ("tpane CPU s", lambda r: r["cpu_seconds"], "{:.3f}"),
        ("main CPU s", lambda r: r["threads"].get("main", 0), "{:.3f}"),
        ("CPU/key ms", lambda r: r["cpu_ms_per_key"], "{:.3f}"),
        ("CPU/s ms", lambda r: r["cpu_ms_per_second"], "{:.3f}"),
        ("frames", lambda r: r["frames"], "{:.0f}"),
        ("bytes", lambda r: r["bytes"], "{:.0f}"),
        ("latency p95 ms", lambda r: r["latency_p95_ms"], "{:.3f}"),
    )
    for scenario, records in by_scenario.items():
        good = [r for r in records if "error" not in r]
        failed = [r for r in records if "error" in r]
        if not good:
            print(f"\n{scenario}: all runs failed ({failed[0]['error']})")
            continue
        binaries = sorted({r["binary"] for r in good})
        print(f"\n{scenario}: {good[0]['note']}")
        header = "  " + f"{'metric':>16}" + "".join(
            f"{Path(b).name:>16}" for b in binaries)
        if len(binaries) == 2:
            header += f"{'delta':>10}"
        print(header)
        for label, fn, fmt in metrics:
            values = []
            for binary in binaries:
                rows = [r for r in good if r["binary"] == binary and fn(r) is not None]
                values.append(statistics.median(fn(r) for r in rows) if rows else None)
            if all(value is None for value in values):
                continue
            line = "  " + f"{label:>16}" + "".join(
                f"{fmt.format(value):>16}" if value is not None else f"{'-':>16}"
                for value in values)
            if len(binaries) == 2 and values[0] and values[1] is not None:
                line += f"{100 * (values[1] - values[0]) / values[0]:>9.1f}%"
            print(line)
        for record in failed:
            print(f"  failed {Path(record['binary']).name}: {record['error']}")


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", type=Path, action="append", required=True)
    parser.add_argument("--scenarios", default=",".join(SCENARIOS))
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    selected = [SCENARIOS[name] for name in args.scenarios.split(",") if name]
    for workload in selected:
        missing = [name for name in ("git",) if not shutil.which(name)]
        if missing:
            raise SystemExit(f"{workload.name}: missing {missing}")
    samples = []
    for trial in range(args.trials):
        for workload in selected:
            for binary in args.binary:
                signal.signal(signal.SIGALRM, hard_timeout)
                signal.setitimer(signal.ITIMER_REAL, workload.timeout + 20)
                try:
                    record = run_workload(workload, binary)
                except Exception as error:
                    record = {"scenario": workload.name, "binary": str(binary),
                              "error": f"{type(error).__name__}: {error}"}
                finally:
                    signal.setitimer(signal.ITIMER_REAL, 0)
                samples.append(record)
                marker = record.get("error", "timed_out" if record.get("timed_out") else "ok")
                detail = (f"{record.get('frames', 0)} frames, "
                          f"{record.get('bytes', 0)} bytes, "
                          f"prep {record.get('prepare_s', 0):.1f}s, "
                          f"stop {record.get('teardown_s', 0):.1f}s")
                stamp = time.strftime("%H:%M:%S")
                print(f"{stamp} trial {trial + 1}/{args.trials} {workload.name} "
                      f"{binary.name}: {marker} "
                      f"({record.get('seconds', 0):.1f}s, {detail}, "
                      f"wall {record.get('wall_s', 0):.1f}s)", flush=True)
    if args.output:
        args.output.write_text(json.dumps({
            "scenarios": {w.name: w.note for w in selected},
            "binaries": [str(b) for b in args.binary],
            "trials": args.trials,
            "tools": tool_versions(),
            "samples": samples,
        }, indent=2, sort_keys=True) + "\n")
    print()
    report(samples)


if __name__ == "__main__":
    sys.exit(main())
