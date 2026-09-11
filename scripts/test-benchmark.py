#!/usr/bin/env python3
import importlib.util
from pathlib import Path
import re
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('benchmark', Path(__file__).with_name('benchmark.py'))
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)
MARKER = re.compile(rb'TPB_ACK_(\d{10})_(\d{8})_END')
ACK = b'TPB_ACK_0000000042_00000001_END'
FRAME = b'\x1b[?2026h' + ACK + bench.SYNC_END


class Stream(bench.HostStream):
    def __init__(self, chunks):
        super().__init__(42, None)
        self.chunks = iter(chunks)
        self.retained = []
        self.reads = 0

    def read(self, timeout):
        self.retained.append(len(self.buffer))
        data = next(self.chunks)
        with patch.object(bench.select, 'select', return_value=([42], [], [])), \
                patch.object(bench.os, 'read', return_value=data):
            super().read(timeout)
        self.reads += 1


class HostStreamTests(unittest.TestCase):
    def test_every_split_preserves_marker_and_frame_completion(self):
        for split in range(1, len(FRAME)):
            with self.subTest(split=split):
                stream = Stream([FRAME[:split], FRAME[split:]])
                match, stamp = stream.wait(MARKER, 5)
                self.assertEqual(match.groups(), (b'0000000042', b'00000001'))
                self.assertEqual(stamp, stream.last_read_ns)
                self.assertEqual(stream.reads, 2)
                self.assertEqual(stream.buffer, b'')

    def test_byte_at_a_time_waits_for_entire_terminator(self):
        stream = Stream([bytes([byte]) for byte in FRAME])
        stream.wait(MARKER, 5)
        self.assertEqual(stream.reads, len(FRAME))

    def test_discards_checked_frames_but_preserves_pending_prefix(self):
        noise = b'\x1b[?2026h' + b'x' * 1024 + bench.SYNC_END
        stream = Stream([noise] * 2048 + [noise + FRAME[:20], FRAME[20:]])
        match, _ = stream.wait(MARKER, 5)
        self.assertEqual(match.group(), ACK)
        self.assertLessEqual(max(stream.retained), 20)
        self.assertGreater(stream.bytes_read, 1024 * 1024)

    def test_preserves_following_frames_and_returns_first_match(self):
        second = FRAME.replace(b'00000001', b'00000002')
        stream = Stream([FRAME + second])
        first, stamp = stream.wait(MARKER, 5)
        self.assertEqual(first.group(2), b'00000001')
        self.assertEqual(stream.buffer, second)
        following, next_stamp = stream.wait(MARKER, 5)
        self.assertEqual(following.group(2), b'00000002')
        self.assertEqual(stamp, next_stamp)
        self.assertEqual(stream.reads, 1)

    def test_match_without_terminator_is_not_discarded(self):
        stream = Stream([FRAME[:-len(bench.SYNC_END)], b'unrelated', bench.SYNC_END])
        match, _ = stream.wait(MARKER, 5)
        self.assertEqual(match.group(), ACK)
        self.assertEqual(stream.reads, 3)


if __name__ == '__main__':
    unittest.main()
