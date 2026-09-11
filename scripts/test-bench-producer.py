#!/usr/bin/env python3
"""Unit tests and short PTY protocol checks; no multiplexer/performance benchmark.

Run: python3 scripts/test-bench-producer.py
"""

import fcntl
import importlib.util
import json
import os
from pathlib import Path
import pty
import select
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time
import unittest
from unittest.mock import Mock, patch


SCRIPT = Path(__file__).with_name("bench-producer.py")
spec = importlib.util.spec_from_file_location("bench_producer", SCRIPT)
producer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(producer)


class PureTests(unittest.TestCase):
    def test_shared_clock_uses_posix_not_process_local_epoch(self):
        with patch.object(producer.time, "clock_gettime_ns", return_value=123) as clock, \
                patch.object(producer.time, "perf_counter_ns", side_effect=AssertionError("local epoch")):
            self.assertEqual(producer.clock_ns(), 123)
        clock.assert_called_once_with(time.CLOCK_MONOTONIC)

    def test_completion_retries_full_driver_receive_queue(self):
        control = Mock()
        record = {"kind": "done"}
        data = json.dumps(record).encode("ascii")
        control.sendto.side_effect = [BlockingIOError(), len(data)]
        with patch.object(producer.select, "select", return_value=([], [control], [])):
            producer.send_done(control, "/driver.sock", record, 1)
        self.assertEqual(control.sendto.call_count, 2)
        control.sendto.assert_called_with(data, "/driver.sock")

    def test_completion_backpressure_timeout_is_not_silent(self):
        control = Mock()
        control.sendto.side_effect = BlockingIOError()
        with patch.object(producer.select, "select", return_value=([], [], [])), \
                self.assertRaises(TimeoutError):
            producer.send_done(control, "/driver.sock", {"kind": "done"}, 1)

    def test_block_and_exact_chunk_tails(self):
        block = producer.output_block()
        self.assertEqual(len(block), 65536)
        self.assertEqual(len(block.splitlines(keepends=True)), 1024)
        self.assertIn(b"request=000000", block[:64])
        self.assertIn(b"request=001023", block[-64:])
        self.assertTrue(all(byte < 128 for byte in block))
        for total in (1, 63, 64, 16383, 16384, 16385, 65535, 65536, 65537, 131091):
            with self.subTest(total=total):
                chunks = list(producer.payload_chunks(block, total))
                self.assertTrue(all(0 < len(chunk) <= 16384 for chunk in chunks))
                self.assertEqual(sum(map(len, chunks)), total)
                self.assertEqual(b"".join(chunks), (block * (total // len(block) + 1))[:total])

    def test_invalid_commands_and_monotonic_generations(self):
        valid = dict(generation=2, payload_bytes=1, start_ns=1234)
        self.assertEqual(producer.parse_command(json.dumps(valid), 1), valid)
        invalid = [b"", b"{", b"\xff", b"null", b"[]", b"1", b"true", b"{}"]
        for field in valid:
            missing = valid.copy()
            del missing[field]
            invalid.append(json.dumps(missing))
            for value in (None, True, False, "2", 1.5, -1):
                invalid.append(json.dumps(dict(valid, **{field: value})))
        invalid.extend(json.dumps(dict(valid, **{field: 0}))
                       for field in ("generation", "payload_bytes"))
        for data in invalid:
            with self.subTest(data=data):
                self.assertIsNone(producer.parse_command(data, 0))
        self.assertIsNone(producer.parse_command(json.dumps(valid), 2))
        self.assertIsNone(producer.parse_command(json.dumps(valid), 3))
        self.assertIsNotNone(producer.parse_command(json.dumps(dict(valid, generation=8)), 2))

    def test_fragmented_and_mismatched_reports(self):
        parser = producer.CursorReports()
        self.assertEqual(parser.feed(b"noise\x1b["), [])
        self.assertEqual(parser.feed(b"1;3"), [])
        self.assertEqual(parser.feed(b"5R\x1b[2;35R\x1b[1;"), [(1, 35), (2, 35)])
        self.assertEqual(parser.feed(b"35R"), [(1, 35)])
        self.assertEqual(parser.feed(b"R"), [])  # No replay of an earlier match.
        self.assertEqual(parser.feed(b"a" * 10000 + b"\x1b[1;"), [])
        self.assertLessEqual(len(parser.pending), 22)
        self.assertEqual(parser.feed(b"35R"), [(1, 35)])


class PTYTests(unittest.TestCase):
    def setUp(self):
        # Short path also fits macOS sockaddr_un.sun_path.
        self.directory = tempfile.TemporaryDirectory(prefix="tpb-", dir="/tmp")
        self.addCleanup(self.directory.cleanup)
        self.driver = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        self.addCleanup(self.driver.close)
        self.driver_path = os.path.join(self.directory.name, "driver.sock")
        self.driver.bind(self.driver_path)
        self.master, slave = pty.openpty()
        self.addCleanup(os.close, self.master)
        self.resize(slave, 80, 24)
        env = dict(os.environ, TPANE_BENCH_READY_DIR=self.directory.name,
                   TPANE_BENCH_CONTROL=self.driver_path, TPANE_BENCH_TIMEOUT="0.3")
        try:
            self.child = subprocess.Popen([sys.executable, str(SCRIPT), "-l"],
                                          stdin=slave, stdout=slave, stderr=slave, env=env)
        finally:
            os.close(slave)
        self.addCleanup(self.stop)
        self.address = os.path.join(self.directory.name, f"producer-{self.child.pid}.sock")
        self.read_until(f"TPB_READY_{self.child.pid:010d}_080x024_END".encode())
        self.assertTrue(Path(self.address).exists())
        self.assertEqual(Path(self.address).stat().st_mode & 0o777, 0o600)
        self.assertEqual(Path(self.directory.name, f"child-{self.child.pid}").read_text(), "80 24\n")

    def stop(self):
        if self.child.poll() is None:
            self.child.terminate()
        self.child.wait(timeout=3)

    @staticmethod
    def resize(fd, cols, rows):
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    def read_until(self, token, timeout=3):
        data = b""
        deadline = time.monotonic() + timeout
        while token not in data:
            remaining = deadline - time.monotonic()
            self.assertGreater(remaining, 0, data)
            self.assertTrue(select.select([self.master], [], [], remaining)[0], data)
            data += os.read(self.master, 65536)
        return data

    def send(self, generation, total=17, start_ns=None):
        command = dict(generation=generation, payload_bytes=total,
                       start_ns=producer.clock_ns() if start_ns is None else start_ns)
        self.driver.sendto(json.dumps(command).encode(), self.address)
        return command

    def assert_no_done(self, seconds=0):
        self.assertFalse(select.select([self.driver], [], [], seconds)[0])

    def finish(self, generation):
        marker = f"TPB_DONE_{self.child.pid:010d}_{generation:08d}_END"
        column = len(marker) + 1
        os.write(self.master, f"\x1b[1;{column}R".encode())
        self.assertTrue(select.select([self.driver], [], [], 2)[0])
        data, sender = self.driver.recvfrom(65536)
        self.assertEqual(sender, self.address)
        result = json.loads(data)
        self.assertEqual(result["dsr"], [1, column])
        return result

    def test_future_start_exact_payload_reply_barrier_and_generations(self):
        self.resize(self.master, 100, 30)
        start_ns = producer.clock_ns() + 120_000_000
        command = self.send(1, 65536 + 19, start_ns)
        self.assertFalse(select.select([self.master], [], [], 0.03)[0])
        data = self.read_until(b"\x1b[6n")
        marker = f"TPB_DONE_{self.child.pid:010d}_00000001_END"
        self.assertEqual(data, b"\x1b[2J\x1b[1;1H" + producer.output_block() + producer.output_block()[:19]
                         + f"\x1b[2J\x1b[1;1H{marker}\x1b[6n".encode())
        self.assert_no_done(0.01)
        self.send(2)  # A command received while waiting for DSR must not queue a phase.
        os.write(self.master, b"\x1b[2;35R\x1b[1;34R")
        self.assert_no_done(0.01)
        os.write(self.master, b"\x1b[1;")
        self.assert_no_done(0.01)
        os.write(self.master, f"{len(marker) + 1}R".encode())
        self.assertTrue(select.select([self.driver], [], [], 2)[0])
        result = json.loads(self.driver.recv(65536))
        self.assertEqual(result["kind"], "done")
        self.assertEqual(result["pid"], self.child.pid)
        self.assertEqual(result["generation"], 1)
        self.assertEqual(result["payload_bytes"], command["payload_bytes"])
        self.assertEqual((result["cols"], result["rows"]), (100, 30))
        self.assertEqual(result["dsr"], [1, len(marker) + 1])
        self.assertGreaterEqual(result["started_ns"], start_ns)
        self.assertGreaterEqual(result["completed_ns"], result["started_ns"])
        self.assertFalse(select.select([self.master], [], [], 0.02)[0])
        self.send(1)  # Completed generations cannot be replayed.
        self.driver.sendto(b"not-json", self.address)
        self.send(2, 1)
        self.read_until(b"\x1b[6n")
        self.assertEqual(self.finish(2)["generation"], 2)
        os.write(self.master, b"xx")
        self.read_until(f"TPB_ACK_{self.child.pid:010d}_00000002_END".encode())
        os.write(self.master, b"q")
        self.assertEqual(self.child.wait(timeout=2), 0)
        self.assertFalse(Path(self.address).exists())

    def test_missing_report_never_sends_done_and_timeout_consumes_generation(self):
        self.send(1)
        self.read_until(b"\x1b[6n")
        self.assert_no_done(0.4)  # Longer than the fixture's bounded DSR wait.
        self.send(1)
        self.send(3)
        self.read_until(b"\x1b[6n")
        self.assertEqual(self.finish(3)["generation"], 3)
        self.stop()
        self.assertFalse(Path(self.address).exists())


if __name__ == "__main__":
    unittest.main()
