#!/usr/bin/env python3
"""Read-only per-thread/kernel-counter diagnostics for the 24/8 busy workload.

Application binary is uninstrumented, but these boundary-sampled runs are marked
profiled and excluded from ordinary performance comparisons. Requires macOS.
"""
import argparse
import ctypes
import importlib.util
import json
import os
from pathlib import Path
import sys

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]


class ThreadInfo(ctypes.Structure):
    _fields_ = [(name, ctypes.c_uint64) for name in ('user', 'system')] + [
        (name, ctypes.c_int32) for name in (
            'cpu_usage', 'policy', 'run_state', 'flags', 'sleep_time',
            'curpri', 'priority', 'maxpriority')
    ] + [('name', ctypes.c_char * 64)]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.binary = args.binary.resolve(strict=True)
    args.output = args.output.resolve()
    if args.output.exists():
        parser.error('refusing to overwrite diagnostic output')
    args.output.parent.mkdir(parents=True, exist_ok=True)
    spec = importlib.util.spec_from_file_location('busy', ROOT / 'scripts/benchmark-busy.py')
    busy = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(busy)
    parent = busy.TaskCPU
    # TaskCPU was imported from benchmark.py; reuse its ABI definition.
    TaskInfo = parent.snapshot.__globals__['TaskInfo']
    if ctypes.sizeof(ThreadInfo) != 112:
        raise RuntimeError('unexpected Darwin proc_threadinfo layout')

    class CPU(parent):
        def snapshot(self, pid):
            result = super().snapshot(pid)
            if pid == os.getpid():  # Base constructor's independent CPU-unit check.
                return result
            ids = (ctypes.c_uint64 * (result['threads'] + 16))()
            n = self.lib.proc_pidinfo(pid, 6, 0, ids, ctypes.sizeof(ids))
            if n <= 0 or n % 8 or n >= ctypes.sizeof(ids):
                raise RuntimeError(f'PROC_PIDLISTTHREADS returned {n}')
            rows = []
            for handle in ids[:n // 8]:
                info = ThreadInfo()
                size = self.lib.proc_pidinfo(pid, 5, handle, ctypes.byref(info), ctypes.sizeof(info))
                if size != ctypes.sizeof(info):
                    raise RuntimeError(f'PROC_PIDTHREADINFO returned {size}; errno={ctypes.get_errno()}')
                rows.append({'handle': handle, 'name': info.name.decode('utf-8', 'replace'),
                             'user_raw': info.user, 'system_raw': info.system,
                             'user_ns': info.user,
                             'system_ns': info.system})
            result['thread_samples'] = rows
            info = TaskInfo()
            size = self.lib.proc_pidinfo(pid, 4, 0, ctypes.byref(info), ctypes.sizeof(info))
            if size != ctypes.sizeof(info):
                raise RuntimeError('task counters unavailable')
            result['kernel_counters'] = {name: getattr(info, 'pti_' + name) & 0xffffffff
                                         for name in ['syscalls_mach', 'syscalls_unix', 'csw']}
            return result

    busy.TaskCPU = CPU
    sys.argv = ['benchmark-busy.py', '--binary', str(args.binary), '--panes', '24', '--busy', '8',
                '--trials', '1', '--seconds', '5', '--keys', '80', '--output', str(args.output)]
    try:
        busy.main()
    finally:
        if args.output.exists():
            data = json.loads(args.output.read_text())
            data['profile'] = {'tool': 'libproc boundary snapshots', 'performance_comparison_valid': False,
                               'thread_time_units': 'nanoseconds; validated against process delta below',
                               'limitations': 'non-atomic thread snapshots; steady thread set required; counters do not attribute CPU to functions'}
            args.output.write_text(json.dumps(data, indent=2) + '\n')
    summaries = []
    for trial in data['trials']:
        phase = trial['steady']
        before, after = phase['cpu_before'], phase['cpu_after']
        a = {row['handle']: row for row in before['thread_samples']}
        b = {row['handle']: row for row in after['thread_samples']}
        if a.keys() != b.keys():
            raise RuntimeError('thread set changed during measurement')
        rows = []
        for handle, row in b.items():
            user = row['user_ns'] - a[handle]['user_ns']
            system = row['system_ns'] - a[handle]['system_ns']
            if min(user, system) < 0:
                raise RuntimeError('non-monotonic thread CPU')
            rows.append({'handle': handle, 'name': row['name'], 'user_seconds': user / 1e9,
                         'system_seconds': system / 1e9, 'cpu_seconds': (user + system) / 1e9})
        total = sum(row['cpu_seconds'] for row in rows)
        process = phase['cpu_total_seconds']
        if abs(total - process) > max(.005, process * .05):
            raise RuntimeError(f'thread CPU unit/scope validation failed: thread sum={total}, process={process}')
        summaries.append({'threads': rows, 'thread_cpu_sum': total, 'process_cpu': process,
                          'kernel_counter_deltas': {k: (after['kernel_counters'][k] - before['kernel_counters'][k]) & 0xffffffff
                                                    for k in before['kernel_counters']}})
    data['profile']['thread_time_units'] = 'nanoseconds (unlike process task Mach ticks); thread sum matched process delta within max(5ms, 5%)'
    data['thread_summary'] = summaries
    args.output.write_text(json.dumps(data, indent=2) + '\n')
    print(json.dumps(summaries, indent=2))


if __name__ == '__main__':
    main()
