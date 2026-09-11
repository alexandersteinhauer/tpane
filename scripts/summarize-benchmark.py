#!/usr/bin/env python3
"""Summarize completed benchmark JSON files; report medians across trials."""
import json
import statistics
import sys
from pathlib import Path

print('| Variant | Panes | Idle CPU % | Key p50 ms | Key p95 ms | Burst ms | Output MiB/s | Output CPU ms/MiB |')
print('|---|---:|---:|---:|---:|---:|---:|---:|')
for name in sys.argv[1:]:
    path = Path(name)
    data = json.loads(path.read_text())
    if data['status'] != 'complete':
        raise SystemExit(f'Incomplete benchmark: {path}')
    for panes in sorted({trial['panes'] for trial in data['trials']}):
        phases = [trial['phases'] for trial in data['trials'] if trial['panes'] == panes]
        def median(phase, field):
            return statistics.median(row[phase][field] for row in phases)
        output_cost = statistics.median(
            row['finite_output']['cpu_total_seconds'] * 1000
            / (row['finite_output']['payload_bytes'] / 1048576) for row in phases)
        print(f'| {path.stem} | {panes} | '
              f'{median("idle", "cpu_percent_one_core"):.3f} | '
              f'{median("input_to_render", "p50_ms"):.2f} | '
              f'{median("input_to_render", "p95_ms"):.2f} | '
              f'{median("paste_burst", "catchup_ms"):.2f} | '
              f'{median("finite_output", "payload_mib_per_second"):.2f} | '
              f'{output_cost:.2f} |')
