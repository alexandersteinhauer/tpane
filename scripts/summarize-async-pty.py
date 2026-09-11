#!/usr/bin/env python3
"""Compare completed PTY-driver benchmarks; medians across trials, RSS not peak."""
import json
import statistics
import sys
from pathlib import Path

print('| Variant | Panes | Key p50 ms | Key p95 ms | Idle CPU % | Threads | Idle RSS MiB | Output MiB/s | Output CPU ms/MiB |')
print('|---|---:|---:|---:|---:|---:|---:|---:|---:|')
for name in sys.argv[1:]:
    path = Path(name)
    data = json.loads(path.read_text())
    if data['status'] != 'complete':
        raise SystemExit(f'Incomplete benchmark: {path}')
    for panes in sorted({trial['panes'] for trial in data['trials']}):
        phases = [trial['phases'] for trial in data['trials'] if trial['panes'] == panes]
        def median(phase, field):
            return statistics.median(row[phase][field] for row in phases)
        threads = statistics.median(row['idle']['cpu_after']['threads'] for row in phases)
        rss = statistics.median(row['idle']['cpu_after']['resident_bytes'] for row in phases) / 1048576
        cost = statistics.median(row['finite_output']['cpu_total_seconds'] * 1000
                                 / (row['finite_output']['payload_bytes'] / 1048576) for row in phases)
        print(f'| {path.stem} | {panes} | '
              f'{median("input_to_render", "p50_ms"):.2f} | '
              f'{median("input_to_render", "p95_ms"):.2f} | '
              f'{median("idle", "cpu_percent_one_core"):.3f} | '
              f'{threads:g} | {rss:.2f} | '
              f'{median("finite_output", "payload_mib_per_second"):.2f} | {cost:.2f} |')
