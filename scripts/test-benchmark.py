#!/usr/bin/env python3
import contextlib
import importlib.util
from pathlib import Path
import re
import struct
import sys
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, call, patch

spec = importlib.util.spec_from_file_location('benchmark', Path(__file__).with_name('benchmark.py'))
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)
sys.path.insert(0, str(Path(__file__).resolve().parent))
busy_spec = importlib.util.spec_from_file_location('benchmark_busy', Path(__file__).with_name('benchmark-busy.py'))
busy = importlib.util.module_from_spec(busy_spec)
busy_spec.loader.exec_module(busy)
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


class ExplorerNavigationTests(unittest.TestCase):
    def test_new_sibling_opens_explorer_and_dismisses_with_n(self):
        self.assertEqual(bench.TPANE_NEW_SIBLING, b'\x00n')

    def test_selection_uses_root_home_then_leaf_offset_not_digit_or_screen_column(self):
        for index in (0, 9, 10, 23, 99, 1000):
            with self.subTest(index=index):
                stream = Mock()
                stream.write.side_effect = range(100, 200)
                self.assertEqual(bench.select_pane(stream, index), 100)
                writes = [c.args[0] for c in stream.write.call_args_list]
                self.assertEqual(b''.join(writes), b'\x00\x1b[H' + b'\x1b[B' * (index + 1) + b'\r')
                self.assertTrue(all(len(data) <= 1024 for data in writes))

    def test_negative_index_is_rejected_before_opening_explorer(self):
        stream = Mock()
        with self.assertRaises(ValueError):
            bench.select_pane(stream, -1)
        stream.write.assert_not_called()


class LegacyStartupTests(unittest.TestCase):
    def test_both_harnesses_create_arbitrary_siblings_at_full_host_size(self):
        class SetupComplete(Exception):
            pass

        for module in (bench, busy):
            for panes in (1, 24, 100):
                with self.subTest(harness=module.__name__, panes=panes), contextlib.ExitStack() as stack:
                    def patched(target, name, **kwargs):
                        return stack.enter_context(patch.object(target, name, **kwargs))

                    args = SimpleNamespace(binary=Path('/fake/tpane'), output_bytes=4096,
                                           panes=panes, busy=min(12, panes - 1), timeout=1,
                                           settle_seconds=0)
                    pids = list(range(300, 300 + panes))
                    process = Mock(pid=100)
                    stream = Mock()
                    # Explorer may redraw the old terminal while opening; old
                    # READY must not count as another created child.
                    records = [pid for pid in pids for _ in range(2)]
                    stream.wait.side_effect = [(module.READY.fullmatch(
                        f'TPB_READY_{pid:010d}_100x028_END'.encode()), 123) for pid in records]
                    stream.drain.side_effect = SetupComplete
                    patched(module.pty, 'openpty', return_value=(900001, 900002))
                    patched(module.tty, 'setraw')
                    ioctl = patched(module.fcntl, 'ioctl')
                    close = module.os.close
                    patched(module.os, 'close', side_effect=lambda fd: None if fd in (900001, 900002) else close(fd))
                    patched(module.subprocess, 'Popen', return_value=process)
                    patched(module, 'CheckedStream' if module is busy else 'HostStream', return_value=stream)
                    patched(module, 'process_table', return_value=[(pid, 100) for pid in pids])
                    patched(module, 'command_output', return_value='')
                    patched(module, 'cleanup')
                    result = {}
                    with self.assertRaises(SetupComplete):
                        if module is busy:
                            module.run_trial(args, result, Mock(), '#!/python\n# inert fixture\n')
                        else:
                            module.run_trial(args, panes, 1, Mock(), '#!/python\n# inert fixture\n', [])
                    ioctl.assert_called_once_with(900002, module.termios.TIOCSWINSZ,
                                                  struct.pack('HHHH', 28, 100, 0, 0))
                    self.assertEqual(stream.write.call_args_list, [call(b'\x00n')] * (panes - 1))
                    self.assertEqual(stream.wait.call_count, panes * 2 - 1)
                    if module is busy:
                        self.assertEqual(result['responder'], pids[-1])
                        self.assertEqual(result['producers'], pids[:args.busy])
                        self.assertEqual([r['ready_viewport'] for r in result['children']],
                                         [[100, 28]] * panes)


if __name__ == '__main__':
    unittest.main()
