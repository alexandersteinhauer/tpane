#!/usr/bin/env python3
"""Pure unit tests for bench.py; no builds, multiplexers, or CPU workloads.

Run: python3 scripts/test-bench.py
"""
import contextlib
import importlib.util
import io
import itertools
import json
from pathlib import Path
import re
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, call, patch

# Resolve the sibling module even when loaded by a test runner from another cwd.
SCRIPTS = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPTS))
spec = importlib.util.spec_from_file_location("standalone_bench", SCRIPTS / "bench.py")
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)

ACK = b"TPB_ACK_0000000042_00000001_END"
ACK_PATTERN = re.compile(rb"TPB_ACK_(\d{10})_(\d{8})_END")
FIELDS = ("elapsed_seconds", "cpu_seconds", "key_p95_seconds")


@contextlib.contextmanager
def fake_producer_source():
    # run_case copies this file, but pure orchestration tests never execute it.
    # Do not require the real producer (or any of its dependencies) to exist.
    original = Path.read_text

    def read_text(path, *args, **kwargs):
        if path == bench.ROOT / "scripts/bench-producer.py":
            return "#!/usr/bin/env python3\n# inert unit-test fixture\n"
        return original(path, *args, **kwargs)

    with patch.object(Path, "read_text", read_text):
        yield


def fake_output_phase(control, root, children, host, selected_pid, generation,
                      payload_bytes, timeout):
    return {"elapsed_seconds": .25, "payload_bytes": payload_bytes * len(children),
            "payload_bytes_per_pane": payload_bytes, "output_start_ns": 1000000000,
            "output_completed_ns": 1250000000, "producer_samples": []}


class ArgumentsTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.output = Path(self.directory.name) / "results.json"

    def parse(self, *args):
        with patch.object(sys, "argv", ["bench.py", "--output", str(self.output), *args]), \
                patch.object(sys, "executable", "/python/bin/python3"):
            return bench.arguments()

    def test_defaults(self):
        args = self.parse()
        self.assertEqual(args.panes, [1, 8, 24])
        self.assertEqual(args.variants, list(bench.VARIANTS))
        self.assertEqual((args.trials, args.keys, args.timeout), (3, 30, 60))
        self.assertEqual(args.output_bytes, 32 * 1048576)
        self.assertEqual(args.output, self.output.resolve())
        self.assertFalse(self.output.exists())

    def test_valid_custom_values_and_canonical_variant_order(self):
        args = self.parse("--panes", "24,1,8", "--variants", "zellij,native,tmux",
                          "--trials", "1", "--keys", "99999999", "--mib", "0.5",
                          "--timeout", ".125")
        self.assertEqual(args.panes, [24, 1, 8])
        self.assertEqual(args.variants, ["native", "tmux", "zellij"])
        self.assertEqual(args.output_bytes, 524288)
        self.assertEqual(args.timeout, .125)

    def test_single_byte_payload_and_native_only_are_valid(self):
        args = self.parse("--mib", str(1 / 1048576), "--variants", "native")
        self.assertEqual(args.output_bytes, 1)
        self.assertEqual(args.variants, ["native"])

    def test_rejects_invalid_cli_values(self):
        invalid = {
            "--panes": ("", "0", "-1", "1,1", "1,", "1,,2", "one", "1.5"),
            "--variants": ("", "tmux", "native,native", "native,unknown", "native,", "Native"),
            "--trials": ("0", "-1", "1.5", "nan"),
            "--keys": ("0", "-1", "100000000", "1.5"),
            "--mib": ("0", "-1", "nan", "inf", "-inf", "1e-10", "1e308"),
            "--timeout": ("0", "-1", "nan", "inf", "-inf"),
        }
        for option, values in invalid.items():
            for value in values:
                with self.subTest(option=option, value=value), \
                        contextlib.redirect_stderr(io.StringIO()) as errors, \
                        self.assertRaises(SystemExit) as exit:
                    self.parse(f"{option}={value}")
                self.assertEqual(exit.exception.code, 2)
                self.assertIn("error:", errors.getvalue())

    def test_existing_output_is_rejected_without_modification(self):
        self.output.write_text("keep me")
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as exit:
            self.parse()
        self.assertEqual(exit.exception.code, 2)
        self.assertEqual(self.output.read_text(), "keep me")

    def test_whitespace_in_python_shebang_is_rejected(self):
        with patch.object(sys, "argv", ["bench.py", "--output", str(self.output)]), \
                patch.object(sys, "executable", "/path with space/python3"), \
                contextlib.redirect_stderr(io.StringIO()) as errors, \
                self.assertRaises(SystemExit) as exit:
            bench.arguments()
        self.assertEqual(exit.exception.code, 2)
        self.assertIn("shebang", errors.getvalue())

    def test_default_output_is_absolute_json_path(self):
        with patch.object(sys, "argv", ["bench.py"]), \
                patch.object(sys, "executable", "/python/bin/python3"), \
                patch.object(bench, "ROOT", Path(self.directory.name)):
            args = bench.arguments()
        self.assertEqual(args.output.parent, Path(self.directory.name).resolve() / ".tools/bench")
        self.assertRegex(args.output.name, r"^run-\d{8}-\d{6}-\d{6}\.json$")
        self.assertFalse(args.output.exists())


class NormalizationTests(unittest.TestCase):
    def setUp(self):
        self.samples = []
        # Paired ratios are [10, 2, 1], median 2. Ratio of medians is 1;
        # ratio of sums is 12/11, so neither aggregate-first shortcut is valid.
        for panes, baselines in ((1, [1, 10, 100]), (8, [100, 1, 10])):
            for trial, (native, ratio) in enumerate(zip(baselines, [10, 2, 1]), 1):
                for variant, value in (("native", native), ("tmux", native * ratio)):
                    self.samples.append({"panes": panes, "trial": trial, "variant": variant,
                                         **{field: value * (index + 1)
                                            for index, field in enumerate(FIELDS)}})
        self.samples.reverse()  # Pair by identifiers, not row position.

    def test_median_of_paired_ratios_for_each_metric_and_pane_count(self):
        for panes in (1, 8):
            for field in FIELDS:
                with self.subTest(panes=panes, field=field):
                    self.assertEqual(bench.normalized(self.samples, panes, "tmux", field), 2)

    def test_native_is_exactly_one_for_every_metric_and_pane_count(self):
        for panes in (1, 8):
            for field in FIELDS:
                with self.subTest(panes=panes, field=field):
                    self.assertEqual(bench.normalized(self.samples, panes, "native", field), 1)

    def test_even_trial_count_uses_middle_pair_average(self):
        samples = [row for row in self.samples if row["trial"] != 3]
        self.assertEqual(bench.normalized(samples, 1, "tmux", FIELDS[0]), 6)

    def test_missing_native_pair_fails_instead_of_reusing_another_trial(self):
        samples = [row for row in self.samples
                   if not (row["variant"] == "native" and row["panes"] == 1 and row["trial"] == 2)]
        with self.assertRaises(KeyError):
            bench.normalized(samples, 1, "tmux", FIELDS[0])

    def test_zero_or_nonfinite_native_cost_is_not_a_valid_denominator(self):
        for value in (0, -1, float("nan"), float("inf")):
            samples = [{**row, FIELDS[0]: value} if row["variant"] == "native" else row
                       for row in self.samples]
            with self.subTest(value=value), self.assertRaisesRegex(RuntimeError, "Unmeasurable"):
                bench.normalized(samples, 1, "tmux", FIELDS[0])

    def test_summary_transposes_metrics_with_a_table_per_pane_count(self):
        report = {"options": {"panes": [1, 8], "variants": ["native", "tmux"]},
                  "samples": self.samples}
        with contextlib.redirect_stdout(io.StringIO()) as output:
            bench.summary(report)
        text = output.getvalue()
        for title in ("1 tab", "8 tabs"):
            self.assertIn(f"### {title}\n", text)
        self.assertEqual(text.count("| Metric | Native | tmux |"), 2)
        self.assertEqual(text.count("|---|---:|---:|"), 2)
        for metric in ("Output time", "CPU cost", "Key p95 (quiet)"):
            self.assertEqual(text.count(f"| {metric} | 1.00× | 2.00× |"), 2)

    def test_summary_labels_all_four_implementations(self):
        samples = self.samples + [{**row, "variant": variant}
                                 for row in self.samples if row["variant"] == "tmux"
                                 for variant in ("tpane", "zellij")]
        report = {"options": {"panes": [1], "variants": list(bench.VARIANTS)},
                  "samples": samples}
        with contextlib.redirect_stdout(io.StringIO()) as output:
            bench.summary(report)
        self.assertIn("| Metric | Native | Us | tmux | Zellij |", output.getvalue())
        self.assertIn("| CPU cost | 1.00× | 2.00× | 2.00× | 2.00× |", output.getvalue())


class CPUTests(unittest.TestCase):
    def test_snapshot_includes_each_pid_once_in_deterministic_order(self):
        cpu = Mock()
        cpu.snapshot.side_effect = lambda pid: {"user_ns": pid, "system_ns": 0}
        result = bench.cpu_snapshot(cpu, {77, 5, 42})
        self.assertEqual(cpu.snapshot.call_args_list, [call(5), call(42), call(77)])
        self.assertEqual(list(result), ["5", "42", "77"])
        self.assertEqual(result["42"]["user_ns"], 42)

    def test_delta_sums_user_and_system_of_all_processes_not_threads_or_memory(self):
        before = {str(pid): {"user_ns": 100, "system_ns": 200, "threads": 64,
                            "resident_bytes": 10**12, "sample_ns": 999}
                  for pid in (10, 20, 30)}
        after = {str(pid): {"user_ns": 100 + pid * 1000000,
                           "system_ns": 200 + pid * 2000000,
                           "threads": 128, "resident_bytes": 10**15, "sample_ns": 10**18}
                 for pid in (30, 10, 20)}
        self.assertAlmostEqual(bench.cpu_delta(before, after), .18)
        self.assertEqual(bench.cpu_delta(before, before), 0)

    def test_each_regressing_counter_is_rejected_even_if_total_increases(self):
        for field in ("user_ns", "system_ns"):
            with self.subTest(field=field):
                before = {"1": {"user_ns": 10, "system_ns": 10}}
                after = {"1": {"user_ns": 100, "system_ns": 100, field: 9}}
                with self.assertRaisesRegex(RuntimeError, "Non-monotonic"):
                    bench.cpu_delta(before, after)

    def test_disappeared_process_is_not_silently_ignored(self):
        with self.assertRaises(KeyError):
            bench.cpu_delta({"1": {"user_ns": 1, "system_ns": 2}}, {})

    def test_descendants_include_entire_tree_but_not_unrelated_processes(self):
        table = [(30, 20), (40, 30), (20, 10), (10, 1), (50, 1), (60, 50)]
        with patch.object(bench, "process_table", return_value=table) as query:
            self.assertEqual(bench.descendants(10), {10, 20, 30, 40})
        query.assert_called_once_with()

    def fake_case(self, variant, missing=None):
        # Startup helpers 101/201 disappear during warmup. Required client 100,
        # detached server 200 and host 400 stay alive; fixtures are 300/301.
        # All process/PTY operations are fake, only config files are real.
        args = SimpleNamespace(output_bytes=100, timeout=1, keys=1)
        process = Mock(pid=100)
        host = Mock(process=Mock(pid=400))
        host.send.return_value = 1000000000

        def wait(pattern, timeout):
            marker = (b"TPB_READY_0000000301_100x028_END"
                      if pattern is bench.READY else pattern.pattern)
            return pattern.fullmatch(marker), 1100000000

        host.wait.side_effect = wait
        cpu = Mock()
        counts = {}
        warmed_up = False

        def snapshot(pid):
            if pid not in (100, 200, 400) or pid == missing:
                raise ProcessLookupError(f"proc_pidinfo({pid}) returned 0")
            count = counts.get(pid, 0)
            counts[pid] = count + 1
            return {"user_ns": count * 1000000, "system_ns": count * 2000000}

        def sleep(seconds):
            nonlocal warmed_up
            if seconds == .1:
                warmed_up = True

        def table():
            if warmed_up:
                return [(100, 1), (200, 1), (300, 200), (301, 200), (999, 1)]
            return [(100, 1), (101, 100), (200, 1), (201, 200), (300, 200), (301, 201), (999, 1)]

        def servers(args):
            session = popen.call_args.kwargs["cwd"].name
            return f"200 /zellij --server /tmp/{session}/socket"

        cpu.snapshot.side_effect = snapshot
        close = bench.os.close
        with fake_producer_source(), \
                patch.object(bench.socket, "socket") as socket_factory, \
                patch.object(bench, "output_phase", side_effect=fake_output_phase) as output_phase, \
                patch.object(bench.pty, "openpty", return_value=(900001, 900002)), \
                patch.object(bench.tty, "setraw"), patch.object(bench.fcntl, "ioctl"), \
                patch.object(bench.os, "close", side_effect=lambda fd: None if fd in (900001, 900002) else close(fd)), \
                patch.object(bench.os, "kill"), patch.object(bench.time, "sleep", side_effect=sleep), \
                patch.object(bench.subprocess, "Popen", return_value=process) as popen, \
                patch.object(bench.subprocess, "run"), \
                patch.object(bench.subprocess, "check_output", return_value=b"200\n"), \
                patch.object(bench, "Host", return_value=host), \
                patch.object(bench, "child_records", return_value={300: (100, 28), 301: (100, 28)}), \
                patch.object(bench, "process_table", side_effect=table), \
                patch.object(bench, "command_output", side_effect=servers):
            result = bench.run_case(args, {variant: Path(f"/fake/{variant}")}, Path("/fake/host"),
                                    variant, 2, 1, cpu)
        host.close.assert_called_once_with()
        self.assertEqual(output_phase.call_count, 2)
        root = popen.call_args.kwargs["cwd"]
        self.assertEqual(output_phase.call_args_list, [
            call(socket_factory.return_value, root, {300: (100, 28), 301: (100, 28)},
                 host, 301, generation, 100, 1) for generation in (1, 2)])
        self.assertEqual(result["elapsed_seconds"], .25)
        self.assertEqual(result["payload_bytes"], 200)
        self.assertEqual(result["payload_bytes_per_pane"], 100)
        return result, cpu

    def test_startup_helpers_exiting_during_warmup_do_not_pollute_cpu_set(self):
        for variant in ("tmux", "zellij"):
            with self.subTest(variant=variant):
                result, cpu = self.fake_case(variant)
                self.assertEqual(result["measured_pids"], [100, 200, 400])
                self.assertEqual(result["fixture_pids"], [300, 301])
                self.assertEqual(cpu.snapshot.call_args_list, [call(pid) for pid in [100, 200, 400] * 2])
                self.assertAlmostEqual(result["cpu_seconds"], .009)

    def test_required_client_server_or_host_exit_still_invalidates_trial(self):
        for pid in (100, 200, 400):
            with self.subTest(pid=pid), contextlib.redirect_stderr(io.StringIO()), \
                    self.assertRaises(ProcessLookupError):
                self.fake_case("zellij", missing=pid)


class OutputPhaseTests(unittest.TestCase):
    ROOT = Path("/fake/case")
    PIDS = {300, 301, 302}
    GENERATION = 7
    PAYLOAD = 4096
    START = 1100000000
    NOW = 1400000000

    def done(self, pid, **changes):
        marker = f"TPB_DONE_{pid:010d}_{self.GENERATION:08d}_END"
        return {"kind": "done", "pid": pid, "generation": self.GENERATION,
                "payload_bytes": self.PAYLOAD, "cols": 100, "rows": 28,
                "dsr": [1, len(marker) + 1], "started_ns": self.START + 100,
                "completed_ns": self.START + 100000000, **changes}

    def marker(self, pid=302, generation=GENERATION):
        return f"TPB_DONE_{pid:010d}_{generation:08d}_END".encode()

    def validate(self, record, sender=None):
        with patch.object(bench, "clock_ns", return_value=self.NOW):
            bench.validate_done(record, sender or str(self.ROOT / "producer-300.sock"),
                                self.ROOT, self.PIDS, self.GENERATION, self.PAYLOAD, self.START)

    def test_validate_done_accepts_exact_completion(self):
        self.validate(self.done(300))

    def test_validate_done_rejects_wrong_sender(self):
        for sender in ("/other/producer-300.sock", str(self.ROOT / "producer-301.sock"),
                       str(self.ROOT / "producer-0300.sock")):
            with self.subTest(sender=sender), self.assertRaisesRegex(RuntimeError, "Invalid producer"):
                self.validate(self.done(300), sender)

    def test_validate_done_rejects_invalid_fields(self):
        changes = [
            {"kind": "ready"}, {"pid": 999}, {"pid": "300"},
            {"generation": self.GENERATION - 1}, {"generation": self.GENERATION + 1},
            {"payload_bytes": self.PAYLOAD - 1}, {"payload_bytes": self.PAYLOAD * 3},
            {"cols": 99}, {"rows": 29}, {"dsr": [2, 33]}, {"dsr": [1, 1]},
            {"dsr": "1;33"}, {"started_ns": self.START - 1},
            {"completed_ns": self.START}, {"completed_ns": self.NOW + 1},
            {"started_ns": float(self.START)}, {"completed_ns": float(self.NOW)},
            {"started_ns": True}, {"completed_ns": None},
        ]
        for change in changes:
            with self.subTest(change=change), self.assertRaisesRegex(RuntimeError, "Invalid producer"):
                self.validate({**self.done(300), **change})
        for key in self.done(300):
            record = self.done(300)
            del record[key]
            with self.subTest(missing=key), self.assertRaisesRegex(RuntimeError, "Invalid producer"):
                self.validate(record)

    @contextlib.contextmanager
    def phase(self, events):
        """Deterministic fake datagrams/host pipe/clock; never touch a real FD."""
        control = Mock()
        control.sendto.side_effect = lambda data, address: len(data)
        host = bench.Host.__new__(bench.Host)
        host.buffer = b""
        host.process = SimpleNamespace(stdout=Mock())
        events = iter(events)
        ticks = itertools.count()
        pop_marker = host.pop_marker

        def assert_queued():
            self.assertEqual(control.sendto.call_count, len(self.PIDS),
                             "all producer commands must precede any completion wait")

        def pop(pattern):
            assert_queued()
            return pop_marker(pattern)

        def select(readers, writers, errors, timeout):
            assert_queued()
            self.assertEqual(readers, [control, host.process.stdout])
            self.assertGreater(timeout, 0)
            event = next(events, None)
            if event is None:
                return [], [], []
            kind, value = event
            if kind == "host":
                host.read.side_effect = lambda: setattr(host, "buffer", host.buffer + value + b"\n")
                return [host.process.stdout], [], []
            record = value
            control.recvfrom.return_value = (json.dumps(record).encode(),
                                             str(self.ROOT / f"producer-{record['pid']}.sock"))
            return [control], [], []

        with patch.object(host, "pop_marker", side_effect=pop), \
                patch.object(host, "read"), \
                patch.object(bench.select, "select", side_effect=select), \
                patch.object(bench.time, "monotonic", side_effect=lambda: next(ticks) / 10), \
                patch.object(bench, "clock_ns",
                             side_effect=itertools.chain([1000000000, 1000000000],
                                                         itertools.repeat(self.NOW))):
            yield control, host

    def run_phase(self, control, host):
        return bench.output_phase(control, self.ROOT, {pid: (100, 28) for pid in self.PIDS},
                                  host, 302, self.GENERATION, self.PAYLOAD, 1)

    def test_queues_identical_commands_before_wait_and_requires_all_completions(self):
        # The selected terminal finishes first; hidden terminals are not optional.
        records = [self.done(302), self.done(300), self.done(301)]
        events = [("done", records[0]), ("host", self.marker()),
                  ("done", records[1]), ("done", records[2])]
        with self.phase(events) as (control, host):
            result = self.run_phase(control, host)
        sends = control.sendto.call_args_list
        self.assertEqual([c.args[1] for c in sends],
                         [str(self.ROOT / f"producer-{pid}.sock") for pid in sorted(self.PIDS)])
        for sent in sends:
            self.assertEqual(json.loads(sent.args[0]), {"generation": self.GENERATION,
                             "payload_bytes": self.PAYLOAD, "start_ns": self.START})
        self.assertEqual(len({c.args[0] for c in sends}), 1)
        self.assertEqual(control.recvfrom.call_args_list, [call(65536)] * 3)
        self.assertEqual(result["producer_samples"], sorted(records, key=lambda r: r["pid"]))
        self.assertEqual(result["payload_bytes"], 3 * self.PAYLOAD)
        self.assertEqual(result["payload_bytes_per_pane"], self.PAYLOAD)
        self.assertEqual(result["output_start_ns"], self.START)
        self.assertEqual(result["output_completed_ns"], self.NOW)
        self.assertAlmostEqual(result["elapsed_seconds"], .3)

    def test_missing_any_child_times_out_even_with_selected_host_done(self):
        for missing in sorted(self.PIDS):
            events = [("host", self.marker())] + [("done", self.done(pid))
                      for pid in sorted(self.PIDS - {missing})]
            with self.subTest(missing=missing), self.phase(events) as (control, host), \
                    self.assertRaisesRegex(TimeoutError, str(missing)):
                self.run_phase(control, host)

    def test_all_child_records_still_require_correct_selected_host_done(self):
        for marker in (None, self.marker(300), self.marker(generation=self.GENERATION - 1)):
            events = [("done", self.done(pid)) for pid in sorted(self.PIDS)]
            if marker is not None:
                events.append(("host", marker))
            with self.subTest(marker=marker), self.phase(events) as (control, host), \
                    self.assertRaisesRegex(TimeoutError, "visible DONE received=False"):
                self.run_phase(control, host)

    def test_host_done_can_arrive_after_all_producer_records(self):
        events = [("done", self.done(pid)) for pid in sorted(self.PIDS)] + [("host", self.marker())]
        with self.phase(events) as (control, host):
            result = self.run_phase(control, host)
        self.assertEqual(len(result["producer_samples"]), 3)

    def test_short_start_command_is_rejected_without_waiting(self):
        with self.phase([]) as (control, host):
            control.sendto.side_effect = lambda data, address: len(data) - 1
            with self.assertRaisesRegex(RuntimeError, "Short producer start command"):
                self.run_phase(control, host)
            control.recvfrom.assert_not_called()
            host.pop_marker.assert_not_called()

    def test_missed_common_start_deadline_is_rejected_without_waiting(self):
        with self.phase([]) as (control, host), \
                patch.object(bench, "clock_ns", side_effect=[1000000000, self.START]):
            with self.assertRaisesRegex(RuntimeError, "Missed common producer start deadline"):
                self.run_phase(control, host)
            self.assertEqual(control.sendto.call_count, 3)
            control.recvfrom.assert_not_called()
            host.pop_marker.assert_not_called()

    def test_duplicate_completion_is_rejected_not_counted_as_another_child(self):
        events = [("host", self.marker()), ("done", self.done(300)), ("done", self.done(300))]
        with self.phase(events) as (control, host), self.assertRaisesRegex(RuntimeError, "Duplicate"):
            self.run_phase(control, host)

    def test_phase_validates_each_child_including_hidden_children(self):
        for pid in sorted(self.PIDS):
            events = [("host", self.marker())] + [("done", self.done(child,
                      payload_bytes=0 if child == pid else self.PAYLOAD))
                      for child in sorted(self.PIDS)]
            with self.subTest(pid=pid), self.phase(events) as (control, host), \
                    self.assertRaisesRegex(RuntimeError, "Invalid producer"):
                self.run_phase(control, host)

    def test_rejects_nonoverlapping_windows_including_touching_endpoints(self):
        for gap in (0, 1):
            records = [self.done(300, completed_ns=self.START + 1000),
                       self.done(301), self.done(302, started_ns=self.START + 1000 + gap)]
            events = [("host", self.marker())] + [("done", record) for record in records]
            with self.subTest(gap=gap), self.phase(events) as (control, host), \
                    self.assertRaisesRegex(RuntimeError, "did not all overlap"):
                self.run_phase(control, host)


class NativeCaseTests(unittest.TestCase):
    @contextlib.contextmanager
    def case(self, ready_pids=(100, 101, 102), children=None):
        args = SimpleNamespace(output_bytes=4096, timeout=1, keys=1)
        children = children if children is not None else {pid: (100, 28) for pid in (100, 101, 102)}
        processes = [Mock(pid=pid) for pid in (100, 101, 102)]
        host = Mock(process=Mock(pid=400))
        host.send.return_value = 1000000000
        ready = iter(ready_pids)

        def wait(pattern, timeout):
            if pattern is bench.READY:
                pid = next(ready, None)
                if pid is None:
                    raise TimeoutError("missing native READY")
                marker = f"TPB_READY_{pid:010d}_100x028_END".encode()
            else:
                marker = pattern.pattern
            return pattern.fullmatch(marker), 1100000000

        host.wait.side_effect = wait
        cpu = Mock()
        cpu.snapshot.side_effect = [{"user_ns": 0, "system_ns": 0},
                                    {"user_ns": 1000000, "system_ns": 2000000}]
        close = bench.os.close
        fds = [(900001 + 2 * i, 900002 + 2 * i) for i in range(3)]
        with fake_producer_source(), contextlib.ExitStack() as stack:
            def patched(target, name, **kwargs):
                return stack.enter_context(patch.object(target, name, **kwargs))

            openpty = patched(bench.pty, "openpty", side_effect=fds)
            patched(bench.tty, "setraw")
            patched(bench.fcntl, "ioctl")
            patched(bench.os, "close", side_effect=lambda fd: None if 900001 <= fd <= 900006 else close(fd))
            patched(bench.time, "sleep")
            popen = patched(bench.subprocess, "Popen", side_effect=processes)
            host_factory = patched(bench, "Host", return_value=host)
            socket_factory = patched(bench.socket, "socket")
            wait_children = patched(bench, "wait_children", return_value=children)
            output = patched(bench, "output_phase", side_effect=fake_output_phase)
            stop = patched(bench, "stop_case")
            # Any accidental real process discovery/launch is a test failure.
            patched(bench, "process_table", side_effect=AssertionError("real process discovery"))
            patched(bench.subprocess, "run", side_effect=AssertionError("external command"))
            patched(bench.os, "kill", side_effect=AssertionError("real signal"))
            yield SimpleNamespace(args=args, children=children, processes=processes, host=host,
                                  cpu=cpu, fds=fds, openpty=openpty, popen=popen,
                                  host_factory=host_factory, socket_factory=socket_factory,
                                  wait_children=wait_children, output=output, stop=stop)

    def run_case(self, case):
        return bench.run_case(case.args, {}, Path("/fake/host"), "native", 3, 1, case.cpu)

    def test_n_child_ptys_one_host_n_fds_and_only_host_cpu(self):
        # READY order need not match Popen order; input still targets the last child.
        with self.case(ready_pids=(102, 100, 101)) as case:
            result = self.run_case(case)
        self.assertEqual(case.openpty.call_count, 3)
        self.assertEqual(case.popen.call_count, 3)
        root = case.popen.call_args_list[0].kwargs["cwd"]
        for launch, (_, slave) in zip(case.popen.call_args_list, case.fds):
            self.assertEqual(launch.args[0], [str(root / "child.py")])
            self.assertEqual(launch.kwargs["stdin"], slave)
            self.assertEqual(launch.kwargs["stdout"], slave)
        case.host_factory.assert_called_once()
        self.assertEqual(case.host_factory.call_args.args[:4],
                         (Path("/fake/host"), [pair[0] for pair in case.fds], 28, 0))
        self.assertEqual([c for c in case.host.wait.call_args_list if c.args[0] is bench.READY],
                         [call(bench.READY, 1)] * 3)
        case.wait_children.assert_called_once_with(root, 3, 1)
        self.assertEqual(case.output.call_args_list, [
            call(case.socket_factory.return_value, root, case.children, case.host,
                 102, generation, 4096, 1) for generation in (1, 2)])
        self.assertEqual(case.cpu.snapshot.call_args_list, [call(400), call(400)])
        self.assertEqual(result["measured_pids"], [400])
        self.assertEqual(result["fixture_pids"], [100, 101, 102])
        self.assertEqual(result["payload_bytes"], 3 * 4096)
        self.assertAlmostEqual(result["cpu_seconds"], .003)
        case.host.close.assert_called_once_with()
        case.socket_factory.return_value.close.assert_called_once_with()
        for process in case.processes:
            process.wait.assert_called_once_with(timeout=3)

    def test_missing_native_ready_cannot_start_warmup_or_measurement(self):
        with self.case(ready_pids=(100, 102)) as case, contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaisesRegex(TimeoutError, "missing native READY"):
                self.run_case(case)
            case.output.assert_not_called()
            case.cpu.snapshot.assert_not_called()

    def test_duplicate_native_ready_is_not_a_third_ready(self):
        with self.case(ready_pids=(100, 102, 100)) as case, contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaisesRegex(RuntimeError, "Duplicate native READY"):
                self.run_case(case)
            case.output.assert_not_called()

    def test_wrong_child_record_count_rejected_before_output(self):
        for pids in ((100, 102), (100, 101, 102, 103)):
            with self.subTest(pids=pids), self.case(children={pid: (100, 28) for pid in pids}) as case, \
                    contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaisesRegex(RuntimeError, "Wrong child count"):
                    self.run_case(case)
                case.output.assert_not_called()


class CleanupTests(unittest.TestCase):
    def test_tmux_cleanup_only_addresses_private_socket(self):
        process = Mock(pid=100)
        with patch.object(bench, "descendants", return_value={100, 200}), \
                patch.object(bench, "child_records", return_value={300: (100, 28)}), \
                patch.object(bench.subprocess, "run") as run, \
                patch.object(bench.os, "kill") as kill:
            bench.stop_case(process, Mock(), {200}, Path("/private/case"), "tmux",
                            ["/tmux", "-S", "/private/case/mux.sock"], ["/zellij"],
                            "case", {"HOME": "/private/case"})
        self.assertEqual(run.call_args.args[0], ["/tmux", "-S", "/private/case/mux.sock", "kill-server"])
        self.assertEqual({args.args[0] for args in kill.call_args_list}, {100, 200, 300})
        process.wait.assert_called_once_with(timeout=3)

    def test_wedged_client_releases_host_before_force_kill(self):
        process = Mock(pid=100)
        process.wait.side_effect = bench.subprocess.TimeoutExpired("fixture", 3)
        host = Mock()
        order = Mock()
        order.attach_mock(host.close, "close_host")
        order.attach_mock(process.kill, "kill_client")
        with patch.object(bench, "descendants", return_value={100}), \
                patch.object(bench, "child_records", return_value={}), \
                patch.object(bench.os, "kill"):
            bench.stop_case(process, host, set(), Path("/private/case"), "native",
                            [], [], "case", {})
        self.assertEqual(order.mock_calls, [call.close_host(), call.kill_client()])


class HostNotificationTests(unittest.TestCase):
    def test_multi_host_launch_passes_every_master_to_one_process(self):
        errors = Mock()
        with patch.object(bench.subprocess, "Popen") as popen:
            host = bench.Host(Path("/fake/host"), [41, 43, 45], 28, 0, errors)
        popen.assert_called_once_with(["/fake/host", "--multi", "28", "0", "41", "43", "45"],
                                      pass_fds=(41, 43, 45), stdin=bench.subprocess.DEVNULL,
                                      stdout=bench.subprocess.PIPE, stderr=errors)
        self.assertEqual(host.master, 45)

    def host(self, buffer=b""):
        host = bench.Host.__new__(bench.Host)  # Never launch the C host.
        host.process = SimpleNamespace(stdout=Mock())
        host.process.stdout.fileno.return_value = 42
        host.buffer = buffer
        return host

    def test_input_retries_nonblocking_backpressure(self):
        host = self.host()
        host.master = 43
        with patch.object(bench.os, "write", side_effect=[BlockingIOError(), 1]) as write, \
                patch.object(bench.select, "select", return_value=([], [43], [])), \
                patch.object(bench.time, "perf_counter_ns", return_value=123):
            self.assertEqual(host.send(b"x"), 123)
        self.assertEqual(write.call_args_list, [call(43, b"x"), call(43, b"x")])

    def test_input_backpressure_timeout_is_bounded(self):
        host = self.host()
        host.master = 43
        with patch.object(bench.os, "write", side_effect=BlockingIOError()), \
                patch.object(bench.select, "select", return_value=([], [], [])), \
                self.assertRaises(TimeoutError):
            host.send(b"x")

    @contextlib.contextmanager
    def stream(self, chunks, buffer=b""):
        host = self.host(buffer)
        with patch.object(bench.select, "select", return_value=([host.process.stdout], [], [])), \
                patch.object(bench.os, "read", side_effect=chunks) as read, \
                patch.object(bench.time, "monotonic", return_value=0), \
                patch.object(bench.time, "perf_counter_ns", return_value=123456789):
            yield host, read

    def test_every_fragment_boundary_including_before_newline(self):
        frame = ACK + b"\n"
        for split in range(1, len(frame)):
            with self.subTest(split=split), self.stream([frame[:split], frame[split:]]) as (host, read):
                match, stamp = host.wait(ACK_PATTERN, 1)
                self.assertEqual(match.groups(), (b"0000000042", b"00000001"))
                self.assertEqual(stamp, 123456789)
                self.assertEqual(read.call_count, 2)
                self.assertEqual(host.buffer, b"")

    def test_byte_at_a_time(self):
        frame = ACK + b"\n"
        with self.stream([bytes([byte]) for byte in frame]) as (host, read):
            self.assertEqual(host.wait(ACK_PATTERN, 1)[0].group(), ACK)
            self.assertEqual(read.call_count, len(frame))

    def test_ignores_nonmatching_lines_and_requires_full_line_match(self):
        noise = b"\nlog\n" + b"prefix" + ACK + b"\n" + ACK + b"suffix\n"
        with self.stream([noise + ACK[:10], ACK[10:] + b"\n"]) as (host, read):
            self.assertEqual(host.wait(ACK_PATTERN, 1)[0].group(), ACK)
            self.assertEqual(read.call_count, 2)

    def test_preserves_coalesced_notifications_and_partial_next_line(self):
        second = ACK.replace(b"00000001", b"00000002")
        with self.stream([ACK + b"\n" + second + b"\npartial"]) as (host, read):
            self.assertEqual(host.wait(ACK_PATTERN, 1)[0].group(), ACK)
            self.assertEqual(host.buffer, second + b"\npartial")
            self.assertEqual(host.wait(ACK_PATTERN, 1)[0].group(), second)
            self.assertEqual(host.buffer, b"partial")
            read.assert_called_once_with(42, 4096)

    def test_unterminated_marker_at_eof_is_not_completion(self):
        with self.stream([ACK, b""]) as (host, read):
            with self.assertRaisesRegex(RuntimeError, "exited before completion"):
                host.wait(ACK_PATTERN, 1)
            self.assertEqual(read.call_count, 2)

    def test_unterminated_stream_is_bounded(self):
        with self.stream([b"x" * 4096] * 17) as (host, read):
            with self.assertRaisesRegex(RuntimeError, "Invalid host notification stream"):
                host.wait(ACK_PATTERN, 1)
            self.assertEqual(read.call_count, 17)

    def test_deadline_expires_without_reading_or_sleeping(self):
        host = self.host()
        with patch.object(bench.time, "monotonic", side_effect=[0, .5, 1]), \
                patch.object(bench.select, "select", return_value=([], [], [])) as select, \
                patch.object(bench.os, "read") as read:
            with self.assertRaises(TimeoutError):
                host.wait(ACK_PATTERN, 1)
        select.assert_called_once_with([host.process.stdout], [], [], .5)
        read.assert_not_called()


if __name__ == "__main__":
    unittest.main()
