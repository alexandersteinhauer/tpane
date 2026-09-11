#!/usr/bin/env python3
"""Medians for completed, unprofiled busy-pane trials. Throughput includes buffers."""
import json
import statistics
import sys
from pathlib import Path

print('| Variant | Panes/busy | Key p50 ms | Key p95 ms | App CPU % | Accepted MiB/s | App CPU ms/MiB | Threads | Final drain ms |')
print('|---|---:|---:|---:|---:|---:|---:|---:|---:|')
for name in sys.argv[1:]:
    path = Path(name)
    data = json.loads(path.read_text())
    if data['status'] != 'complete' or data.get('profile') or data.get('profiled'):
        raise SystemExit(f'Incomplete or profiled run: {path}')
    trials = data['trials']
    if any(t['status'] != 'complete' for t in trials):
        raise SystemExit(f'Incomplete trial: {path}')
    def median(field):
        return statistics.median(t['steady'][field] for t in trials)
    opts = data['options']
    cost = statistics.median(t['steady']['cpu_total_seconds'] * 1000
                             / (t['steady']['producer_accepted_bytes'] / 1048576) for t in trials)
    threads = statistics.median(t['steady']['cpu_before']['threads'] for t in trials)
    drain = statistics.median(t['drain_complete_ms'] for t in trials)
    print(f'| {path.stem} | {opts["panes"]}/{opts["busy"]} | '
          f'{median("p50_ms"):.2f} | {median("p95_ms"):.2f} | '
          f'{median("cpu_percent_one_core"):.2f} | '
          f'{median("producer_accepted_mib_per_second"):.2f} | '
          f'{cost:.2f} | {threads:g} | {drain:.2f} |')
